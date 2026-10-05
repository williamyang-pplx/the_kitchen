//! Timers plugins register with `kitchen.every`: the host owns the schedule,
//! records every run (kept in SQLite across restarts, see `db.rs`), and never
//! starts a timer while its previous run is still going — also across a
//! reload, when the old Lua state may still be finishing that run.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lock;

/// Wall-clock now, in (fractional) unix seconds.
pub fn wall_now() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Debug, Clone)]
pub struct TimerSpec {
    pub plugin: String,
    pub name: String,
    pub desc: String,
    pub interval: Duration,
    /// Wait before the first run when the timer has never run.
    pub first_delay: Duration,
    /// The plugin load that registered it.
    pub generation: u64,
    /// When that load registered it (unix s).
    pub registered_at: f64,
}

/// What the host remembers about a timer's runs (persisted per plugin + name).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimerState {
    pub runs: u64,
    /// Unix seconds.
    pub last_fired: Option<f64>,
    pub last_finished: Option<f64>,
    pub last_duration_secs: Option<f64>,
    pub last_error: Option<String>,
    /// The last run's stats: a flat map of name → number or string.
    #[serde(default)]
    pub stats: Value,
    /// Per-workspace stats: workspace id → flat map.
    #[serde(default)]
    pub workspaces: Value,
}

/// A run the scheduler (or `t:run()`) started: send it to the plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Fire {
    pub plugin: String,
    pub name: String,
    pub run: u64,
    pub generation: u64,
}

/// A registered timer, for the dashboard and `kitchenctl plugin timers`.
#[derive(Debug, Clone, Serialize)]
pub struct TimerInfo {
    pub plugin: String,
    pub name: String,
    pub desc: String,
    pub interval_secs: f64,
    pub next_due: f64,
    pub running: bool,
    #[serde(flatten)]
    pub state: TimerState,
}

#[derive(Debug)]
struct Running {
    run: u64,
    generation: u64,
}

#[derive(Debug, Default)]
struct Entry {
    state: TimerState,
    running: Option<Running>,
}

#[derive(Default)]
pub struct Timers {
    specs: Mutex<Vec<TimerSpec>>,
    /// Keyed by (plugin, name); kept when a plugin unloads, so a run in
    /// flight stays marked until it ends.
    entries: Mutex<HashMap<(String, String), Entry>>,
    next_run: AtomicU64,
}

fn next_due(spec: &TimerSpec, state: &TimerState) -> f64 {
    match state.last_fired {
        Some(t) => t + spec.interval.as_secs_f64(),
        None => spec.registered_at + spec.first_delay.as_secs_f64(),
    }
}

impl Timers {
    /// Add or replace `(spec.plugin, spec.name)`. `saved` seeds the state the
    /// first time this process sees the timer (it was persisted earlier).
    pub fn register(&self, spec: TimerSpec, saved: Option<TimerState>) {
        let key = (spec.plugin.clone(), spec.name.clone());
        lock(&self.entries).entry(key).or_insert_with(|| Entry { state: saved.unwrap_or_default(), running: None });
        let mut specs = lock(&self.specs);
        specs.retain(|s| !(s.plugin == spec.plugin && s.name == spec.name));
        specs.push(spec);
    }

    /// The plugin unloaded: its timers stop being scheduled.
    pub fn drop_plugin(&self, plugin: &str) {
        lock(&self.specs).retain(|s| s.plugin != plugin);
    }

    /// Start every timer that is due and not running.
    pub fn due(&self, wall: f64) -> Vec<Fire> {
        let specs = lock(&self.specs).clone();
        let mut entries = lock(&self.entries);
        let mut out = Vec::new();
        for spec in specs {
            let Some(e) = entries.get_mut(&(spec.plugin.clone(), spec.name.clone())) else { continue };
            if e.running.is_none() && next_due(&spec, &e.state) <= wall {
                out.push(self.start(&spec, e, wall));
            }
        }
        out
    }

    /// Start `plugin.name` now (a manual run). None while it's running.
    pub fn fire_now(&self, plugin: &str, name: &str, wall: f64) -> Option<Fire> {
        let spec = lock(&self.specs).iter().find(|s| s.plugin == plugin && s.name == name).cloned()?;
        let mut entries = lock(&self.entries);
        let e = entries.get_mut(&(spec.plugin.clone(), spec.name.clone()))?;
        if e.running.is_some() {
            return None;
        }
        Some(self.start(&spec, e, wall))
    }

    fn start(&self, spec: &TimerSpec, e: &mut Entry, wall: f64) -> Fire {
        let run = self.next_run.fetch_add(1, Ordering::Relaxed) + 1;
        e.running = Some(Running { run, generation: spec.generation });
        e.state.runs += 1;
        e.state.last_fired = Some(wall);
        Fire { plugin: spec.plugin.clone(), name: spec.name.clone(), run, generation: spec.generation }
    }

    /// Run `run` ended with the fn's (stats, per_workspace) or an error.
    /// Returns the state to persist; None when `run` is not the one in flight.
    pub fn finish(&self, plugin: &str, name: &str, run: u64, out: Result<(Value, Value), String>, wall: f64) -> Option<TimerState> {
        let mut entries = lock(&self.entries);
        let e = entries.get_mut(&(plugin.to_string(), name.to_string()))?;
        if e.running.as_ref().map(|r| r.run) != Some(run) {
            return None;
        }
        e.running = None;
        e.state.last_finished = Some(wall);
        e.state.last_duration_secs = e.state.last_fired.map(|f| (wall - f).max(0.0));
        match out {
            Ok((stats, workspaces)) => {
                e.state.stats = stats;
                e.state.workspaces = workspaces;
                e.state.last_error = None;
            }
            Err(err) => e.state.last_error = Some(err),
        }
        Some(e.state.clone())
    }

    /// The plugin thread of `generation` ended: runs it never finished end
    /// with an error. Returns (name, state) to persist.
    pub fn release_generation(&self, plugin: &str, generation: u64, wall: f64) -> Vec<(String, TimerState)> {
        let mut entries = lock(&self.entries);
        let mut out = Vec::new();
        for ((p, name), e) in entries.iter_mut() {
            if p == plugin && e.running.as_ref().is_some_and(|r| r.generation == generation) {
                e.running = None;
                e.state.last_finished = Some(wall);
                e.state.last_error = Some("the plugin stopped before the run finished".into());
                out.push((name.clone(), e.state.clone()));
            }
        }
        out
    }

    /// Registered timers, sorted by plugin then name.
    pub fn list(&self) -> Vec<TimerInfo> {
        let specs = lock(&self.specs).clone();
        let entries = lock(&self.entries);
        let mut out: Vec<TimerInfo> = specs
            .iter()
            .filter_map(|s| {
                let e = entries.get(&(s.plugin.clone(), s.name.clone()))?;
                Some(TimerInfo {
                    plugin: s.plugin.clone(),
                    name: s.name.clone(),
                    desc: s.desc.clone(),
                    interval_secs: s.interval.as_secs_f64(),
                    next_due: next_due(s, &e.state),
                    running: e.running.is_some(),
                    state: e.state.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.plugin, &a.name).cmp(&(&b.plugin, &b.name)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(generation: u64, interval: f64, first_delay: f64, at: f64) -> TimerSpec {
        TimerSpec {
            plugin: "p".into(),
            name: "t".into(),
            desc: String::new(),
            interval: Duration::from_secs_f64(interval),
            first_delay: Duration::from_secs_f64(first_delay),
            generation,
            registered_at: at,
        }
    }

    #[test]
    fn schedule_follows_first_delay_then_interval() {
        let t = Timers::default();
        t.register(spec(1, 10.0, 2.0, 100.0), None);
        assert!(t.due(101.0).is_empty());
        let fire = t.due(102.0);
        assert_eq!(fire.len(), 1);
        assert!(t.due(200.0).is_empty(), "running: never twice at once");
        assert!(t.fire_now("p", "t", 200.0).is_none());
        let state = t.finish("p", "t", fire[0].run, Ok((json!({"removed": 1}), json!({}))), 105.0).unwrap();
        assert_eq!((state.runs, state.last_duration_secs, state.stats["removed"].as_i64()), (1, Some(3.0), Some(1)));
        assert!(t.due(111.9).is_empty());
        assert_eq!(t.due(112.0).len(), 1, "interval after the last fire");
    }

    #[test]
    fn saved_state_sets_the_next_run() {
        let t = Timers::default();
        let saved = TimerState { runs: 7, last_fired: Some(1000.0), ..Default::default() };
        t.register(spec(1, 3600.0, 60.0, 2000.0), Some(saved));
        assert!(t.due(2060.0).is_empty(), "ran at 1000: next at 4600, not 60 s after a restart");
        assert_eq!(t.list()[0].next_due, 4600.0);
        assert_eq!(t.due(4600.0).len(), 1);
        assert_eq!(t.list()[0].state.runs, 8);
    }

    #[test]
    fn a_run_survives_a_reload_until_it_ends() {
        let t = Timers::default();
        t.register(spec(1, 1.0, 0.0, 0.0), None);
        let old = t.due(0.0).remove(0);
        // Reload: the plugin re-registers under generation 2 while run `old` goes on.
        t.drop_plugin("p");
        t.register(spec(2, 1.0, 0.0, 5.0), None);
        assert!(t.due(10.0).is_empty(), "the old load's run is still in flight");
        assert!(t.finish("p", "t", old.run + 99, Ok((Value::Null, Value::Null)), 10.0).is_none(), "not the run in flight");
        assert!(t.finish("p", "t", old.run, Err("boom".into()), 10.0).unwrap().last_error.is_some());
        let next = t.due(11.0).remove(0);
        assert_eq!(next.generation, 2);
        // The new load's thread ends mid-run: the run is released with an error.
        assert!(t.release_generation("p", 1, 11.5).is_empty());
        let released = t.release_generation("p", 2, 11.5);
        assert_eq!(released.len(), 1);
        assert!(t.due(20.0).len() == 1);
    }
}
