//! Plugins: neovim-style, hackable Lua (one state per plugin, each on its own
//! thread) plus external processes for heavy work. See docs/plugins-design.md.
//!
//! The UI never calls into Lua: it sends [`ToPlugin`] messages and reads
//! cached state (the [`Registry`] of commands/keymaps and the
//! [`decor::DecorStore`]). The host tracks everything a plugin registers, so
//! a reload (or crash) cleans up without the plugin's help.

pub mod dashboard;
pub mod decor;
pub mod jobs;
pub mod keys;
mod lua;
pub mod procs;
pub mod timers;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use kitchen_core::util::content_hash;
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use dashboard::DashboardStore;
use decor::{DecorStore, FileKey};
use keys::Chord;
use timers::{Fire, TimerInfo, TimerSpec, TimerState, Timers};

/// The plugin API level this build implements.
pub const API_LEVEL: u32 = 12;
/// Events the kitchen hands to the plugins that listen (`kitchen.on`):
/// `open` (a link typed into ⌘⇧P, with the tab's command ctx), `comment`
/// (a viewer comment to post), `dashboard` (a workspace's dashboard was shown
/// or hidden), `views` (⌘⇧S wants the tab's PR menu: the tab's command ctx
/// plus `prs`, the lines of its PR list, and `branch`, the one checked out).
pub const EMITTED: &[&str] = &["open", "comment", "dashboard", "views"];
/// File text above this is not handed to plugins (`ev.text()` returns nil).
pub const MAX_TEXT: usize = 2 * 1024 * 1024;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ------------------------------------------------------------------ manifest

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "api_one")]
    pub api: u32,
    #[serde(default = "default_entry")]
    pub entry: String,
    /// File events only for these extensions (empty = every file).
    #[serde(default)]
    pub filetypes: Vec<String>,
    #[serde(default)]
    pub config: toml::Table,
}

fn api_one() -> u32 {
    1
}
fn default_entry() -> String {
    "init.lua".into()
}

impl Manifest {
    pub fn wants(&self, filetype: &str) -> bool {
        self.filetypes.is_empty() || self.filetypes.iter().any(|f| f == filetype)
    }
}

/// `~/.kitchen/plugins.toml`: `[name] enabled = false`, other keys override `config`.
fn user_overrides() -> toml::Table {
    let path = kitchen_proto::state_dir().join("plugins.toml");
    std::fs::read_to_string(path).ok().and_then(|s| s.parse::<toml::Table>().ok()).unwrap_or_default()
}

// ------------------------------------------------------------------ registry

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum When {
    Any,
    Terminal,
    Viewer,
    Plan,
}

impl When {
    pub fn parse(s: &str) -> Result<When> {
        Ok(match s {
            "any" | "" => When::Any,
            "terminal" => When::Terminal,
            "viewer" => When::Viewer,
            "plan" => When::Plan,
            _ => bail!("unknown `when` {s:?} (any, terminal, viewer, plan)"),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandEntry {
    pub plugin: String,
    /// `<plugin>.<name>`.
    pub name: String,
    pub desc: String,
    /// May be run by requests that come from a devbox.
    pub remote: bool,
}

#[derive(Debug, Clone)]
pub struct ChordEntry {
    pub plugin: String,
    pub chord: Chord,
    pub command: String,
    pub when: When,
    pub filetype: Option<String>,
}

/// A plugin's terminal link rule: ⌘-click on a `regex` match runs `command`.
#[derive(Debug, Clone)]
pub struct LinkEntry {
    pub plugin: String,
    pub regex: regex::Regex,
    /// `<plugin>.<name>`.
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct ViewerMap {
    pub plugin: String,
    /// Keys after the leader, e.g. `f` for `<leader>f`.
    pub keys: String,
    pub command: String,
}

/// A plugin answers viewer queries such as `gd` (definition) and `gr` (references).
#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub plugin: String,
    pub kind: String,
    /// Extensions it answers for (empty = the manifest's filetypes, or all).
    pub filetypes: Vec<String>,
}

/// Provider kinds the viewer asks for.
pub const PROVIDER_KINDS: &[&str] = &["definition", "references"];

/// Everything plugins registered, snapshotted for the UI.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub commands: Vec<CommandEntry>,
    pub chords: Vec<ChordEntry>,
    pub viewer_maps: Vec<ViewerMap>,
    pub providers: Vec<ProviderEntry>,
    /// (event, plugin) for each [`EMITTED`] event a plugin listens to.
    pub listeners: Vec<(String, String)>,
    /// `kitchen.link` rules, in registration order.
    pub links: Vec<LinkEntry>,
}

/// A place a provider points at. `path` is worktree-relative, or absolute
/// for files outside the worktree (e.g. installed packages).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Location {
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// 1-based character of the raw line.
    #[serde(default = "one")]
    pub col: usize,
    /// Preview text (the viewer fills it in when missing).
    #[serde(default)]
    pub text: Option<String>,
}

fn one() -> usize {
    1
}

/// A provider's answer: `None` = "can't tell" (the viewer falls back to its
/// own search), `Some(vec![])` = "there is none".
pub type ProviderReply = Option<Vec<Location>>;
type ReplyFn = Box<dyn FnOnce(ProviderReply) + Send>;

// ------------------------------------------------------------------ messages

/// A file the viewer loaded (or reloaded after it changed on disk).
#[derive(Debug)]
pub struct FileEvent {
    pub key: FileKey,
    pub filetype: String,
    /// Content hash of `text` (see [`content_hash`]).
    pub version: String,
    pub text: Arc<str>,
    pub changed: bool,
}

/// What a command sees about where it was run from.
pub fn command_context(engine: &Arc<Engine>, tab_id: &str, viewer: Option<serde_json::Value>) -> serde_json::Value {
    let found = engine.find_tab(tab_id);
    serde_json::json!({
        "workspace": found.as_ref().map(|(w, _)| w.row.id.clone()),
        "tab": found.as_ref().map(|(_, t)| t.row.id.clone()),
        "host": found.as_ref().map(|(w, _)| w.host().label().to_string()),
        "worktree": found.as_ref().map(|(_, t)| t.row.worktree.clone()),
        "viewer": viewer,
    })
}

pub fn filetype_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub enum ToPlugin {
    File(Arc<FileEvent>),
    Invoke { command: String, ctx: serde_json::Value },
    JobDone { generation: u64, callback: u64, result: jobs::JobResult },
    /// Answer a viewer query; the plugin replies via [`PluginHost::reply`].
    Provide { kind: String, request: u64, ctx: serde_json::Value },
    /// An [`EMITTED`] event for its `kitchen.on` handlers.
    Event { name: String, value: serde_json::Value },
    ProcOut { generation: u64, proc: u64, stream: procs::Stream, data: Vec<u8> },
    ProcExit { generation: u64, proc: u64, code: i32 },
    /// Run timer `name` (a `kitchen.every` fn); report back with [`PluginHost::timer_done`].
    Timer { generation: u64, name: String, run: u64 },
    Stop,
}

// ------------------------------------------------------------------ host

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "detail")]
pub enum Status {
    Running,
    Disabled,
    Failed(String),
}

struct Slot {
    name: String,
    dir: PathBuf,
    manifest: Option<Manifest>,
    tx: Option<Sender<ToPlugin>>,
    generation: u64,
    status: Status,
    /// Source files' signature when last loaded.
    sig: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub dir: String,
    pub status: Status,
    pub commands: Vec<String>,
}

pub struct PluginHost {
    engine: Weak<Engine>,
    dirs: Vec<PathBuf>,
    slots: Mutex<Vec<Slot>>,
    next_generation: AtomicU64,
    pub decor: DecorStore,
    pub timers: Timers,
    pub dashboard: DashboardStore,
    scheduler: AtomicBool,
    registry: Mutex<Arc<Registry>>,
    registry_gen: AtomicU64,
    /// Latest text of every file a viewer holds, replayed after a reload.
    open_files: Mutex<HashMap<FileKey, Arc<FileEvent>>>,
    toasted: Mutex<HashSet<String>>,
    /// Provider requests awaiting a reply: id -> (plugin, reply).
    requests: Mutex<HashMap<u64, (String, ReplyFn)>>,
    next_request: AtomicU64,
}

impl PluginHost {
    /// Discover and load every plugin, and start the reload watcher.
    pub fn start(engine: &Arc<Engine>, dirs: Vec<PathBuf>) -> Arc<PluginHost> {
        let host = Arc::new(PluginHost {
            engine: Arc::downgrade(engine),
            dirs,
            slots: Mutex::new(Vec::new()),
            next_generation: AtomicU64::new(1),
            decor: DecorStore::default(),
            timers: Timers::default(),
            dashboard: DashboardStore::default(),
            scheduler: AtomicBool::new(false),
            registry: Mutex::new(Arc::new(Registry::default())),
            registry_gen: AtomicU64::new(0),
            open_files: Mutex::new(HashMap::new()),
            toasted: Mutex::new(HashSet::new()),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
        });
        crate::viewer::highlight::set_extra_syntax_dirs(host.discover().iter().map(|(_, d)| d.join("syntaxes")).filter(|d| d.is_dir()).collect());
        host.rescan();
        let weak = Arc::downgrade(&host);
        std::thread::Builder::new()
            .name("plugin-watch".into())
            .spawn(move || watch(weak))
            .ok();
        host
    }

    /// For tests: no discovery, no watcher.
    #[cfg(test)]
    pub fn empty(engine: &Arc<Engine>) -> Arc<PluginHost> {
        Arc::new(PluginHost {
            engine: Arc::downgrade(engine),
            dirs: Vec::new(),
            slots: Mutex::new(Vec::new()),
            next_generation: AtomicU64::new(1),
            decor: DecorStore::default(),
            timers: Timers::default(),
            dashboard: DashboardStore::default(),
            scheduler: AtomicBool::new(false),
            registry: Mutex::new(Arc::new(Registry::default())),
            registry_gen: AtomicU64::new(0),
            open_files: Mutex::new(HashMap::new()),
            toasted: Mutex::new(HashSet::new()),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
        })
    }

    fn engine(&self) -> Option<Arc<Engine>> {
        self.engine.upgrade()
    }

    /// (name, dir) of every plugin directory (later dirs don't override earlier ones).
    fn discover(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for root in &self.dirs {
            let Ok(rd) = std::fs::read_dir(root) else { continue };
            let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("plugin.toml").is_file()).collect();
            entries.sort();
            for dir in entries {
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, dir));
                }
            }
        }
        out
    }

    /// Load new plugins, unload removed ones, reload changed ones.
    fn rescan(self: &Arc<Self>) {
        let found = self.discover();
        let (loaded, changed): (Vec<String>, Vec<String>) = {
            let slots = lock(&self.slots);
            let loaded = slots.iter().map(|s| s.name.clone()).collect();
            let changed = slots.iter().filter(|s| signature(&s.dir) != s.sig).map(|s| s.name.clone()).collect();
            (loaded, changed)
        };
        for (name, dir) in &found {
            if !loaded.contains(name) {
                self.load(name, dir);
            }
        }
        for name in loaded {
            if !found.iter().any(|(n, _)| *n == name) {
                self.unload(&name);
                lock(&self.slots).retain(|s| s.name != name);
            }
        }
        for name in changed {
            if let Some((_, dir)) = found.iter().find(|(n, _)| *n == name) {
                log::info!("plugin {name}: changed on disk, reloading");
                self.load(&name, dir);
            }
        }
    }

    /// Reload one plugin (or all).
    pub fn reload(self: &Arc<Self>, name: Option<&str>) -> Result<Vec<String>> {
        let found = self.discover();
        let targets: Vec<(String, PathBuf)> = match name {
            Some(n) => vec![found.iter().find(|(f, _)| f == n).cloned().ok_or_else(|| anyhow!("no plugin named {n:?}"))?],
            None => found,
        };
        for (n, d) in &targets {
            self.load(n, d);
        }
        Ok(targets.into_iter().map(|(n, _)| n).collect())
    }

    /// Stop a plugin and drop everything it registered.
    fn unload(&self, name: &str) {
        let mut slots = lock(&self.slots);
        if let Some(s) = slots.iter_mut().find(|s| s.name == name) {
            if let Some(tx) = s.tx.take() {
                let _ = tx.send(ToPlugin::Stop);
            }
            s.generation = 0;
        }
        drop(slots);
        self.edit_registry(|r| {
            r.commands.retain(|c| c.plugin != name);
            r.chords.retain(|c| c.plugin != name);
            r.viewer_maps.retain(|c| c.plugin != name);
            r.providers.retain(|c| c.plugin != name);
            r.listeners.retain(|(_, p)| p != name);
            r.links.retain(|l| l.plugin != name);
        });
        self.decor.clear_owner(name);
        self.timers.drop_plugin(name);
        self.dashboard.clear_owner(name);
        self.fail_requests(name);
    }

    /// (Re)load `name` from `dir`.
    fn load(self: &Arc<Self>, name: &str, dir: &Path) {
        self.unload(name);
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let sig = signature(dir);
        let overrides = user_overrides();
        let user = overrides.get(name).and_then(|v| v.as_table()).cloned().unwrap_or_default();
        let manifest = std::fs::read_to_string(dir.join("plugin.toml"))
            .with_context(|| format!("read {}/plugin.toml", dir.display()))
            .and_then(|s| toml::from_str::<Manifest>(&s).context("parse plugin.toml"));
        let (manifest, status) = match manifest {
            Err(e) => (None, Status::Failed(format!("{e:#}"))),
            Ok(m) if m.api > API_LEVEL => {
                let msg = format!("needs plugin API {} (this kitchen has {API_LEVEL})", m.api);
                (Some(m), Status::Failed(msg))
            }
            Ok(m) if user.get("enabled").and_then(|v| v.as_bool()) == Some(false) => (Some(m), Status::Disabled),
            Ok(m) => (Some(m), Status::Running),
        };
        if let Some(m) = manifest.as_ref().filter(|m| m.name != name) {
            self.log(name, &format!("warning: plugin.toml says name = {:?}; the folder name {name:?} is what counts", m.name));
        }
        let mut tx = None;
        if let (Some(m), Status::Running) = (&manifest, &status) {
            let mut config = m.config.clone();
            for (k, v) in user.iter().filter(|(k, _)| *k != "enabled") {
                config.insert(k.clone(), v.clone());
            }
            let config = serde_json::to_value(&config).unwrap_or_default();
            tx = Some(lua::spawn(self.clone(), name.to_string(), dir.to_path_buf(), m.entry.clone(), config, generation));
        }
        if let Status::Failed(e) = &status {
            self.log(name, &format!("not loaded: {e}"));
            self.report_error(name, &format!("not loaded: {e}"));
        }
        let slot = Slot { name: name.to_string(), dir: dir.to_path_buf(), manifest, tx, generation, status, sig };
        let mut slots = lock(&self.slots);
        match slots.iter_mut().find(|s| s.name == name) {
            Some(s) => *s = slot,
            None => slots.push(slot),
        }
        drop(slots);
        // Replay the files viewers hold so diagnostics come back after a reload.
        let files: Vec<Arc<FileEvent>> = lock(&self.open_files).values().cloned().collect();
        for f in files {
            self.send_file(name, &f);
        }
    }

    fn send_file(&self, name: &str, f: &Arc<FileEvent>) {
        let slots = lock(&self.slots);
        if let Some(s) = slots.iter().find(|s| s.name == name) {
            if let (Some(tx), Some(m)) = (&s.tx, &s.manifest) {
                if m.wants(&f.filetype) {
                    let _ = tx.send(ToPlugin::File(f.clone()));
                }
            }
        }
    }

    /// The viewer loaded `key` (first time, or again after it changed).
    pub fn file_loaded(&self, key: FileKey, text: Arc<str>, changed: bool) {
        let filetype = filetype_of(&key.path);
        let version = content_hash(&text);
        let ev = Arc::new(FileEvent { key: key.clone(), filetype, version, text, changed });
        lock(&self.open_files).insert(key, ev.clone());
        let slots = lock(&self.slots);
        for s in slots.iter() {
            if let (Some(tx), Some(m)) = (&s.tx, &s.manifest) {
                if m.wants(&ev.filetype) {
                    let _ = tx.send(ToPlugin::File(ev.clone()));
                }
            }
        }
    }

    /// Run a plugin command. `remote`: the request came from a devbox.
    pub fn invoke(&self, command: &str, ctx: serde_json::Value, remote: bool) -> Result<()> {
        let reg = self.registry();
        let entry = reg.commands.iter().find(|c| c.name == command).ok_or_else(|| anyhow!("no plugin command {command:?}"))?;
        if remote && !entry.remote {
            bail!("{command} can't be run from a devbox (the plugin didn't mark it remote = true)");
        }
        let slots = lock(&self.slots);
        let tx = slots.iter().find(|s| s.name == entry.plugin).and_then(|s| s.tx.clone()).ok_or_else(|| anyhow!("plugin {} is not running", entry.plugin))?;
        drop(slots);
        tx.send(ToPlugin::Invoke { command: command.to_string(), ctx }).map_err(|_| anyhow!("plugin {} stopped", entry.plugin))
    }

    /// Is there a `kind` provider for files of `filetype`?
    pub fn has_provider(&self, kind: &str, filetype: &str) -> bool {
        self.provider_for(kind, filetype).is_some()
    }

    fn provider_for(&self, kind: &str, filetype: &str) -> Option<ProviderEntry> {
        let reg = self.registry();
        let slots = lock(&self.slots);
        reg.providers
            .iter()
            .filter(|p| p.kind == kind)
            .find(|p| {
                if !p.filetypes.is_empty() {
                    return p.filetypes.iter().any(|f| f == filetype);
                }
                slots.iter().find(|s| s.name == p.plugin).and_then(|s| s.manifest.as_ref()).is_some_and(|m| m.wants(filetype))
            })
            .cloned()
    }

    /// Ask the `kind` provider for `filetype`. Returns false (and never
    /// calls `reply`) when there is none; otherwise `reply` is called
    /// exactly once — with `None` if the plugin fails, reloads or declines.
    pub fn provide(&self, kind: &str, filetype: &str, ctx: serde_json::Value, reply: impl FnOnce(ProviderReply) + Send + 'static) -> bool {
        let Some(p) = self.provider_for(kind, filetype) else { return false };
        let tx = lock(&self.slots).iter().find(|s| s.name == p.plugin).and_then(|s| s.tx.clone());
        let Some(tx) = tx else { return false };
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        lock(&self.requests).insert(id, (p.plugin.clone(), Box::new(reply)));
        if tx.send(ToPlugin::Provide { kind: kind.to_string(), request: id, ctx }).is_err() {
            self.reply(id, None);
        }
        true
    }

    /// A plugin's answer to request `id` (later answers are ignored).
    pub fn reply(&self, id: u64, answer: ProviderReply) {
        let f = lock(&self.requests).remove(&id);
        if let Some((_, f)) = f {
            f(answer);
        }
    }

    /// `plugin`'s pending answers will never come: let the askers fall back.
    fn fail_requests(&self, plugin: &str) {
        let orphaned: Vec<ReplyFn> = {
            let mut reqs = lock(&self.requests);
            let ids: Vec<u64> = reqs.iter().filter(|(_, (p, _))| p == plugin).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| reqs.remove(&id)).map(|(_, f)| f).collect()
        };
        for f in orphaned {
            f(None);
        }
    }

    fn register_listener(&self, plugin: &str, generation: u64, event: &str) {
        if self.current(plugin, generation) {
            let entry = (event.to_string(), plugin.to_string());
            self.edit_registry(|r| {
                if !r.listeners.contains(&entry) {
                    r.listeners.push(entry);
                }
            });
        }
    }

    /// Plugins listening to `event`.
    pub fn listeners(&self, event: &str) -> Vec<String> {
        self.registry().listeners.iter().filter(|(e, _)| e == event).map(|(_, p)| p.clone()).collect()
    }

    /// Hand `value` to every `event` handler. False when no plugin listens.
    pub fn emit(&self, event: &str, value: serde_json::Value) -> bool {
        let listeners = self.listeners(event);
        let slots = lock(&self.slots);
        let mut sent = false;
        for s in slots.iter().filter(|s| listeners.contains(&s.name)) {
            if let Some(tx) = &s.tx {
                sent |= tx.send(ToPlugin::Event { name: event.to_string(), value: value.clone() }).is_ok();
            }
        }
        sent
    }

    fn register_provider(&self, plugin: &str, generation: u64, entry: ProviderEntry) {
        if !self.current(plugin, generation) {
            return;
        }
        self.edit_registry(|r| {
            r.providers.retain(|p| !(p.plugin == entry.plugin && p.kind == entry.kind));
            r.providers.push(entry);
        });
    }

    pub fn registry(&self) -> Arc<Registry> {
        lock(&self.registry).clone()
    }

    fn edit_registry(&self, f: impl FnOnce(&mut Registry)) {
        let mut r = lock(&self.registry);
        let mut next = (**r).clone();
        f(&mut next);
        *r = Arc::new(next);
        drop(r);
        self.registry_gen.fetch_add(1, Ordering::AcqRel);
        if let Some(e) = self.engine() {
            e.ctx.request_repaint();
        }
    }

    /// Is `generation` still the live load of `plugin`? Registrations from
    /// an older load (a callback racing a reload) are ignored.
    fn current(&self, plugin: &str, generation: u64) -> bool {
        lock(&self.slots).iter().any(|s| s.name == plugin && s.generation == generation && s.tx.is_some())
    }

    fn register_command(&self, plugin: &str, generation: u64, entry: CommandEntry) {
        if !self.current(plugin, generation) {
            return;
        }
        self.edit_registry(|r| {
            r.commands.retain(|c| c.name != entry.name);
            r.commands.push(entry);
        });
    }

    fn register_link(&self, plugin: &str, generation: u64, entry: LinkEntry) {
        if !self.current(plugin, generation) {
            return;
        }
        self.edit_registry(|r| {
            r.links.retain(|l| !(l.plugin == entry.plugin && l.regex.as_str() == entry.regex.as_str()));
            r.links.push(entry);
        });
    }

    fn register_chord(&self, plugin: &str, generation: u64, entry: ChordEntry) -> Result<()> {
        if !self.current(plugin, generation) {
            return Ok(());
        }
        if let Some(b) = crate::keymap::builtin_conflict(&entry.chord) {
            bail!("{} is a built-in shortcut ({b})", entry.chord.label());
        }
        if entry.chord.ctrl && !entry.chord.cmd && !entry.chord.alt && matches!(entry.when, When::Any | When::Terminal) {
            bail!("{}: ctrl chords belong to the shell in terminals; use when = \"viewer\" (or add cmd/alt)", entry.chord.label());
        }
        let reg = self.registry();
        if let Some(other) = reg.chords.iter().find(|c| c.chord == entry.chord && c.when == entry.when && c.plugin != plugin) {
            bail!("{} is already mapped by plugin {}", entry.chord.label(), other.plugin);
        }
        self.edit_registry(|r| {
            r.chords.retain(|c| !(c.chord == entry.chord && c.when == entry.when && c.filetype == entry.filetype));
            r.chords.push(entry);
        });
        Ok(())
    }

    fn register_viewer_map(&self, plugin: &str, generation: u64, entry: ViewerMap) {
        if !self.current(plugin, generation) {
            return;
        }
        self.edit_registry(|r| {
            r.viewer_maps.retain(|m| m.keys != entry.keys);
            r.viewer_maps.push(entry);
        });
    }

    fn register_timer(self: &Arc<Self>, spec: TimerSpec) {
        if !self.current(&spec.plugin, spec.generation) {
            return;
        }
        let saved = self
            .engine()
            .and_then(|e| e.db().timer_state(&spec.plugin, &spec.name).ok().flatten())
            .and_then(|s| serde_json::from_str::<TimerState>(&s).ok());
        self.timers.register(spec, saved);
        if !self.scheduler.swap(true, Ordering::AcqRel) {
            let weak = Arc::downgrade(self);
            std::thread::Builder::new().name("plugin-timers".into()).spawn(move || schedule(weak)).ok();
        }
        self.repaint();
    }

    /// Hand a started run to its plugin; if it can't take it, the run ends now.
    fn send_fire(&self, fire: Fire) {
        let tx = lock(&self.slots).iter().find(|s| s.name == fire.plugin && s.generation == fire.generation).and_then(|s| s.tx.clone());
        let sent = tx.is_some_and(|tx| tx.send(ToPlugin::Timer { generation: fire.generation, name: fire.name.clone(), run: fire.run }).is_ok());
        if !sent {
            self.log(&fire.plugin, &format!("timer {}: the plugin is not running", fire.name));
            self.timer_done(&fire.plugin, &fire.name, fire.run, Err("the plugin is not running".into()));
        }
    }

    /// Run `plugin.name` now (`t:run()`); false while a run is in flight.
    fn fire_timer(&self, plugin: &str, name: &str) -> bool {
        match self.timers.fire_now(plugin, name, timers::wall_now()) {
            Some(fire) => {
                self.send_fire(fire);
                true
            }
            None => false,
        }
    }

    /// A timer run ended: record and persist it.
    fn timer_done(&self, plugin: &str, name: &str, run: u64, out: Result<(serde_json::Value, serde_json::Value), String>) {
        if let Some(state) = self.timers.finish(plugin, name, run, out, timers::wall_now()) {
            self.save_timer(plugin, name, &state);
        }
        self.repaint();
    }

    /// The thread of load `generation` ended; runs it held end with an error.
    fn timers_released(&self, plugin: &str, generation: u64) {
        for (name, state) in self.timers.release_generation(plugin, generation, timers::wall_now()) {
            self.save_timer(plugin, &name, &state);
        }
    }

    fn save_timer(&self, plugin: &str, name: &str, state: &TimerState) {
        let (Some(e), Ok(json)) = (self.engine(), serde_json::to_string(state)) else { return };
        if let Err(err) = e.db().put_timer_state(plugin, name, &json) {
            log::warn!("plugin {plugin}: save timer {name}: {err:#}");
        }
    }

    /// Every registered timer and what its runs did.
    pub fn timers_info(&self) -> Vec<TimerInfo> {
        self.timers.list()
    }

    /// The UI showed or hid workspace `ws`'s dashboard; tell the plugins.
    pub fn dashboard_shown(&self, ws: &str, open: bool) {
        if self.dashboard.set_showing(ws, open) {
            self.emit("dashboard", serde_json::json!({ "workspace": ws, "open": open }));
        }
    }

    fn repaint(&self) {
        if let Some(e) = self.engine() {
            e.ctx.request_repaint();
        }
    }

    pub fn list(&self) -> Vec<PluginInfo> {
        let reg = self.registry();
        lock(&self.slots)
            .iter()
            .map(|s| PluginInfo {
                name: s.name.clone(),
                version: s.manifest.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
                description: s.manifest.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
                dir: s.dir.display().to_string(),
                status: s.status.clone(),
                commands: reg.commands.iter().filter(|c| c.plugin == s.name).map(|c| c.name.clone()).collect(),
            })
            .collect()
    }

    fn mark_failed(&self, plugin: &str, generation: u64, msg: String) {
        let mut slots = lock(&self.slots);
        if let Some(s) = slots.iter_mut().find(|s| s.name == plugin && s.generation == generation) {
            s.status = Status::Failed(msg);
            s.tx = None;
        }
        drop(slots);
        self.fail_requests(plugin);
    }

    pub fn log_path(plugin: &str) -> PathBuf {
        kitchen_proto::state_dir().join("logs").join("plugins").join(format!("{plugin}.log"))
    }

    pub fn log(&self, plugin: &str, msg: &str) {
        use std::io::Write;
        let path = Self::log_path(plugin);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Milliseconds, so stage times (e.g. a language server's startup) read off the stamps.
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{stamp}] {msg}");
        }
        log::debug!(target: "kitchen::plugins", "{plugin}: {msg}");
    }

    /// Log an error and toast it once per (plugin, message).
    pub fn report_error(&self, plugin: &str, msg: &str) {
        self.log(plugin, &format!("error: {msg}"));
        let key = format!("{plugin}\0{msg}");
        if lock(&self.toasted).insert(key) {
            if let Some(e) = self.engine() {
                let short: String = msg.lines().next().unwrap_or(msg).chars().take(200).collect();
                e.toast_err(format!("plugin {plugin}: {short}"));
            }
        }
    }
}

/// Signature of a plugin dir's sources (plugin.toml + **/*.lua, symlinks followed).
fn signature(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut files = Vec::new();
    collect_sources(dir, &mut files, 0);
    files.sort();
    for f in files {
        if let Ok(meta) = std::fs::metadata(&f) {
            f.hash(&mut h);
            meta.len().hash(&mut h);
            meta.modified().ok().hash(&mut h);
        }
    }
    h.finish()
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            collect_sources(&p, out, depth + 1);
        } else if name == "plugin.toml" || name.ends_with(".lua") {
            out.push(p);
        }
    }
}

/// Start timers as they come due (never one whose last run is still going).
fn schedule(host: Weak<PluginHost>) {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let Some(h) = host.upgrade() else { return };
        for fire in h.timers.due(timers::wall_now()) {
            h.send_fire(fire);
        }
    }
}

/// Poll plugin dirs; reload a plugin once its sources stop changing (two
/// equal signatures ≥ 300 ms apart), load new ones, unload removed ones.
fn watch(host: Weak<PluginHost>) {
    let mut last: HashMap<PathBuf, (u64, Instant)> = HashMap::new();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let Some(h) = host.upgrade() else { return };
        let found = h.discover();
        let mut stable = true;
        for (_, dir) in &found {
            let sig = signature(dir);
            match last.get(dir) {
                Some((s, at)) if *s == sig && at.elapsed() >= Duration::from_millis(300) => {}
                _ => {
                    last.insert(dir.clone(), (sig, Instant::now()));
                    stable = false;
                }
            }
        }
        if stable {
            h.rescan();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(f: &dyn Fn() -> bool) {
        let start = Instant::now();
        while !f() && start.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(f(), "timed out");
    }

    fn host_with(engine: &Arc<Engine>, dirs: Vec<PathBuf>) -> Arc<PluginHost> {
        let host = PluginHost::empty(engine);
        let host = Arc::new(PluginHost { dirs, ..Arc::try_unwrap(host).ok().unwrap() });
        host.rescan();
        host
    }

    /// markdown: the document is one Markdown row, front matter as a yaml block.
    #[test]
    fn markdown_plugin_rows() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/markdown/lua/markdown.lua");
        let m: mlua::Table = lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap();
        let rows_of = |text: &str| -> Vec<decor::RenderRow> {
            let f: mlua::Function = m.get("rows").unwrap();
            let v: serde_json::Value = lua.from_value(f.call(text).unwrap()).unwrap();
            decor::parse_render(v).unwrap()
        };
        let rows = rows_of("---\ntitle: x\n---\n# Title\n\n- one\n");
        assert_eq!(rows.len(), 1, "one row, for one pass");
        assert_eq!((rows[0].line, rows[0].last_line()), (1, 6), "it stands for every line");
        assert_eq!(rows[0].markdown, "```yaml\ntitle: x\n```\n# Title\n\n- one");
        assert_eq!(rows_of("---\nno end").remove(0).markdown, "---\nno end", "not front matter");
        assert!(rows_of("").is_empty());
    }

    /// github: PR links, the view of a PR, its comment threads and post arguments.
    #[test]
    fn github_plugin_views() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        lua.load("kitchen = { on = function() end, command = function() end, keymap = function() end, config = {}, json = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github/init.lua");
        let m: mlua::Table = lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap();
        let parse: mlua::Function = m.get("parse").unwrap();
        for (text, want) in [
            ("https://github.com/ppl-ai/air/pull/14211", Some(("ppl-ai/air", 14211))),
            ("https://github.com/ppl-ai/air/pull/14211/changes", Some(("ppl-ai/air", 14211))),
            ("github.com/a.b/c_d/pull/7/files?w=1#diff", Some(("a.b/c_d", 7))),
            ("ppl-ai/air#9", Some(("ppl-ai/air", 9))),
            ("https://github.com/ppl-ai/air/issues/14211", None),
            ("hello", None),
        ] {
            let got: (Option<String>, Option<u64>) = parse.call(text).unwrap();
            assert_eq!(got.0.as_deref().zip(got.1), want, "{text}");
        }
        let view: mlua::Function = m.get("view").unwrap();
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let info = |v: serde_json::Value| lua.to_value(&v).unwrap();
        // The PR head, detached, diffed against its base (fetched with it).
        let s = to_json(view.call(("t1", 12, info(serde_json::json!({"baseRefName": "dev", "state": "OPEN"})), "main")).unwrap());
        assert_eq!((s["tab"].as_str(), s["name"].as_str(), s["rev"].as_str(), s["base"].as_str()), (Some("t1"), Some("kitchen-pr-12"), Some("refs/kitchen/pr/12"), Some("origin/dev")));
        assert_eq!(s["fetch"], serde_json::json!(["+refs/pull/12/head:refs/kitchen/pr/12", "+refs/heads/dev:refs/remotes/origin/dev"]));
        assert_eq!(to_json(view.call(("t1", 12, info(serde_json::json!({})), "main")).unwrap())["base"], "origin/main", "no base from gh");
        // Merged: the base just before the merge.
        let s = to_json(view.call(("t1", 12, info(serde_json::json!({"baseRefName": "dev", "state": "MERGED", "mergeCommit": {"oid": "abc"}})), "main")).unwrap());
        assert_eq!((s["base"].as_str(), s["fetch"][1].as_str()), (Some("abc^1"), Some("abc")));
        assert!(s.get("have").is_none(), "no head commit from gh: always fetch");
        // With the commits GitHub reports, the fetch can be skipped when they are here.
        let s = to_json(view.call(("t1", 12, info(serde_json::json!({"baseRefName": "dev", "baseRefOid": "b1", "headRefOid": "h1", "state": "OPEN"})), "main")).unwrap());
        assert_eq!(s["have"], serde_json::json!([{ "rev": "refs/kitchen/pr/12", "commit": "h1" }, { "rev": "refs/remotes/origin/dev", "commit": "b1" }]));
        let s = to_json(view.call(("t1", 12, info(serde_json::json!({"headRefOid": "h1", "state": "MERGED", "mergeCommit": {"oid": "abc"}})), "main")).unwrap());
        assert_eq!(s["have"][1], serde_json::json!({ "rev": "abc", "commit": "abc" }), "merged: the merge commit itself");

        // Review comments: threads from gh's list, items for the viewer, post arguments.
        let list = lua
            .to_value(&serde_json::json!([
                { "id": 2, "in_reply_to_id": 1, "path": "a.py", "user": "bo", "body": "agreed", "created_at": "2026-10-02T00:00:00Z" },
                { "id": 1, "path": "a.py", "line": 40, "side": "RIGHT", "user": "al", "body": "use a set", "created_at": "2026-10-01T00:00:00Z" },
                { "id": 3, "path": "a.py", "original_line": 7, "side": "LEFT", "user": "cy", "body": "old", "created_at": "2026-10-03T00:00:00Z" },
                { "id": 4, "path": "b.py", "line": 1, "user": "dee", "body": "x", "created_at": "2026-10-03T00:00:00Z" },
            ]))
            .unwrap();
        let threads: mlua::Value = m.get::<mlua::Function>("threads").unwrap().call(list).unwrap();
        let items = to_json(m.get::<mlua::Function>("items").unwrap().call((threads, "a.py", "o/r")).unwrap());
        assert_eq!(items.as_array().unwrap().len(), 2, "the reply joins its thread; b.py is elsewhere");
        assert_eq!(items[0]["id"], "1");
        assert_eq!(items[0]["line"], 40);
        assert_eq!(items[0]["side"], "new");
        assert_eq!(items[0]["title"], "@al · L40 · 2026-10-01 · 1 reply");
        assert_eq!(items[0]["body"], "use a set");
        assert_eq!(items[0]["replies"], serde_json::json!([{ "title": "@bo · 2026-10-02", "body": "agreed" }]), "each reply under its own header");
        assert_eq!(items[0]["repo"], "o/r");
        assert_eq!((items[1]["line"].clone(), items[1]["dim"].clone()), (serde_json::Value::Null, serde_json::json!(true)), "outdated: file level, faded");
        let post: mlua::Function = m.get("post_args").unwrap();
        let ev = lua.to_value(&serde_json::json!({ "path": "a.py", "line": 12, "side": "old", "start_line": 10, "start_side": "new", "text": "hm", "quote": "x = 1\ny = 2" })).unwrap();
        let args = to_json(post.call(("o/r", 5, ev.clone(), "abc")).unwrap());
        assert_eq!(
            args,
            serde_json::json!(["api", "-X", "POST", "repos/o/r/pulls/5/comments", "-f", "commit_id=abc", "-f", "path=a.py", "-f", "body=hm", "-F", "line=12", "-f", "side=LEFT", "-F", "start_line=10", "-f", "start_side=RIGHT"])
        );
        let file_level = to_json(post.call(("o/r", 5, ev, "abc", true)).unwrap());
        assert_eq!(file_level[9], "subject_type=file");
        assert_eq!(file_level[11], "body=**L12**: `x = 1`\n\nhm");
    }

    /// github ⌘⇧S: the tab's PR list parsed, one GraphQL query, and the
    /// stacks among the PRs as menu sections (the checked-out one live).
    #[test]
    fn github_plugin_menu() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        lua.load("kitchen = { on = function() end, command = function() end, keymap = function() end, config = {}, json = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github/init.lua");
        let m: mlua::Table = lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap();
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let val = |j: serde_json::Value| lua.to_value(&j).unwrap();

        let lines = val(serde_json::json!(["12", " #13 ", "https://github.com/O/r/pull/14/files", "x/y#15", "junk", "12", "o/r#16"]));
        assert_eq!(to_json(m.get::<mlua::Function>("list").unwrap().call((lines, "o/r")).unwrap()), serde_json::json!([12, 13, 14, 16]));
        let q: String = m.get::<mlua::Function>("query").unwrap().call(("o/r", val(serde_json::json!([12, 13])))).unwrap();
        assert!(q.starts_with(r#"query { repository(owner: "o", name: "r") { p12: pullRequest(number: 12) {"#) && q.contains("p13: pullRequest(number: 13)"), "{q}");

        let pr = |n: u64, base: &str, head: &str, state: &str| serde_json::json!({ "number": n, "title": format!("t{n}"), "state": state, "baseRefName": base, "headRefName": head, "baseRefOid": format!("b{n}"), "headRefOid": format!("h{n}"), "additions": 1, "deletions": 0 });
        // Two stacks (main ← a ← b, and main ← c), listed out of order; a cycle; a reviewed PR.
        let listed = val(serde_json::json!([pr(2, "a", "b", "OPEN"), pr(3, "main", "c", "OPEN"), pr(1, "main", "a", "OPEN"), pr(5, "y", "x", "OPEN"), pr(6, "x", "y", "OPEN")]));
        let reviewing = val(serde_json::json!([pr(9, "main", "bo/fix", "MERGED")]));
        let e = to_json(m.get::<mlua::Function>("entries").unwrap().call(("t1", listed, reviewing, "b", "main")).unwrap());
        let rows: Vec<(&str, &str, &str)> = e.as_array().unwrap().iter().map(|r| (r["section"].as_str().unwrap(), r["label"].as_str().unwrap(), r["detail"].as_str().unwrap())).collect();
        assert_eq!(
            rows,
            vec![
                ("main ← a", "#1 t1", "1/2 · main ← a · +1 −0"),
                ("main ← a", "#2 t2", "2/2 · a ← b · +1 −0 · live"),
                ("main ← c", "#3 t3", "1/1 · main ← c · +1 −0"),
                ("y ← x", "#5 t5", "1/2 · y ← x · +1 −0"),
                ("y ← x", "#6 t6", "2/2 · x ← y · +1 −0"),
                ("Reviewing", "#9 t9", "main ← bo/fix · +1 −0 · merged"),
            ]
        );
        // The live row is the tab's worktree against its parent; the others are checkouts.
        assert_eq!(e[1]["view"], serde_json::json!({ "base": "origin/a" }));
        assert_eq!((e[0]["view"]["name"].as_str(), e[0]["view"]["base"].as_str(), e[0]["view"].get("tab")), (Some("kitchen-pr-1"), Some("origin/main"), None));
        assert_eq!(e[5]["view"]["name"], "kitchen-pr-9");
        assert_eq!(e[0]["view"]["have"][0], serde_json::json!({ "rev": "refs/kitchen/pr/1", "commit": "h1" }), "rows skip the fetch when nothing moved");
    }

    /// github ⌘⇧D: a PR's description as the page the viewer renders, its
    /// body as it is (the viewer renders it as GitHub does).
    #[test]
    fn github_plugin_description() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        lua.load(
            r#"kitchen = { on = function() end, command = function() end, keymap = function() end, config = {}, json = {} }"#,
        )
        .exec()
        .unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github/init.lua");
        let m: mlua::Table = lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap();
        let description: mlua::Function = m.get("description").unwrap();
        let mut info = serde_json::json!({ "number": 12, "title": "Add retries", "author": { "login": "al" }, "state": "OPEN", "isDraft": true,
            "baseRefName": "main", "headRefName": "al/retry", "additions": 40, "deletions": 3, "url": "https://github.com/o/r/pull/12", "body": "" });
        let page: String = description.call(lua.to_value(&info).unwrap()).unwrap();
        assert_eq!(
            page,
            "# Add retries\n\n**#12** · @al · draft · `main` ← `al/retry` · +40 −3\n\n[https://github.com/o/r/pull/12](https://github.com/o/r/pull/12)\n\n---\n\n_No description provided._\n"
        );
        info["body"] = "Fixes #3.\r\n<!-- hint -->".into();
        let page: String = description.call(lua.to_value(&info).unwrap()).unwrap();
        assert!(page.ends_with("---\n\nFixes #3.\n<!-- hint -->\n"), "the body as GitHub has it: {page}");
    }

    fn prune_module(lua: &mlua::Lua) -> mlua::Table {
        lua.load("kitchen = { on = function() end, command = function() end, every = function() return {} end, config = {}, json = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/prune-worktrees/init.lua");
        lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap()
    }

    /// prune-worktrees: which worktrees are candidates, how a probe reads,
    /// what the verdict is, and the guarded branch delete.
    #[test]
    fn prune_plugin_decides_what_to_remove() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let m = prune_module(&lua);
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let val = |j: serde_json::Value| lua.to_value(&j).unwrap();

        let list = val(serde_json::json!([
            { "path": "/r/air", "branch": "main", "head": "a", "role": "main" },
            { "path": "/r/air-kitchen-spare", "head": "b", "role": "spare" },
            { "path": "/r/air-tabbed", "branch": "william/tabbed", "head": "c", "role": "edit", "tab": "t1" },
            { "path": "/r/air-moved", "branch": "william/open-elsewhere", "head": "d", "role": "edit" },
            { "path": "/r/air-named", "head": "e", "role": "edit" },
            { "path": "/r/air-shell", "branch": "william/shell", "head": "f", "role": "edit" },
            { "path": "/r/air-tmp", "branch": "william/tmp", "head": "g", "role": "edit" },
            { "path": "/r/air-good", "branch": "william/good", "head": "h", "role": "edit" },
            { "path": "/r/air-detached", "head": "i", "role": "edit" },
            { "path": "/r/air-plain", "branch": "william/plain", "head": "j", "role": "edit" },
        ]));
        // Tabs whose worktree path git spells differently: matched by branch / name.
        let tabs = val(serde_json::json!([
            { "worktree": "/private/r/air-other", "branch": "william/open-elsewhere" },
            { "worktree": "/private/r/air-named" },
        ]));
        // A pane deep in air-shell, one in air-tmp spelled /private, one in a
        // sibling whose name merely starts with air-good.
        let panes = val(serde_json::json!(["/r/air-shell/src/lib", "/private/r/air-tmp", "/r/air-good2"]));
        let got = to_json(m.get::<mlua::Function>("candidates").unwrap().call((list, tabs, panes)).unwrap());
        assert_eq!(
            got,
            serde_json::json!([
                { "path": "/r/air-good", "branch": "william/good", "head": "h" },
                { "path": "/r/air-detached", "head": "i" },
                { "path": "/r/air-plain", "branch": "william/plain", "head": "j" },
            ])
        );

        let parse: mlua::Function = m.get("parse_probe").unwrap();
        let p = to_json(parse.call("touched 0\ncontained 1\non_default 0\n").unwrap());
        assert_eq!(p, serde_json::json!({ "touched": false, "contained": true, "on_default": false }));
        assert_eq!(to_json(parse.call("touched 0\ncontained 1\n").unwrap()), serde_json::Value::Null, "truncated: the probe failed");

        let verdict: mlua::Function = m.get("verdict").unwrap();
        let judge = |branch: Option<&str>, probe: serde_json::Value| -> (String, String, bool) {
            let mut wt = serde_json::json!({ "path": "/r/x", "head": "abc" });
            if let Some(b) = branch {
                wt["branch"] = b.into();
            }
            let wt = val(wt);
            let probe = if probe.is_null() { mlua::Value::Nil } else { val(probe) };
            let v = to_json(verdict.call((wt, probe, "main")).unwrap());
            (v["action"].as_str().unwrap().into(), v["why"].as_str().unwrap().into(), v["delete_branch"].as_bool().unwrap_or(false))
        };
        let probe = |touched: bool, contained: bool, on_default: bool| serde_json::json!({ "touched": touched, "contained": contained, "on_default": on_default });
        let keep = |why: &str| ("keep".to_string(), why.to_string(), false);
        let remove = |delete: bool| ("remove".to_string(), "idle".to_string(), delete);
        assert_eq!(judge(Some("b"), serde_json::Value::Null), keep("probe failed"));
        assert_eq!(judge(Some("b"), probe(true, true, true)), keep("touched recently"));
        assert_eq!(judge(None, probe(false, false, false)), keep("detached commits on no ref"));
        assert_eq!(judge(None, probe(false, true, true)), remove(false), "detached: no branch to delete");
        assert_eq!(judge(Some("b"), probe(false, true, true)), remove(true));
        assert_eq!(judge(Some("main"), probe(false, true, true)), remove(false), "never the default branch");
        assert_eq!(judge(Some("b"), probe(false, true, false)), remove(false), "unmerged commits stay on the branch");

        let del: String = m.get::<mlua::Function>("delete_branch_sh").unwrap().call(("william/x'y", "abc")).unwrap();
        assert_eq!(del, r#"[ "$(git rev-parse --verify -q 'refs/heads/william/x'\''y')" = 'abc' ] && git branch -D 'william/x'\''y'"#);
    }

    /// prune-worktrees: the probe script against a real repo, so a find/xargs
    /// slip can't read fresh work as idle (it's removed with --force).
    #[test]
    fn prune_probe_reads_a_real_worktree() {
        let lua = mlua::Lua::new();
        let m = prune_module(&lua);
        let script: String = m.get::<mlua::Function>("probe_sh").unwrap().call((4320, "refs/remotes/origin/main")).unwrap();
        let repo = crate::test_util::TestRepo::with_files(&[("a.txt", "a")]);
        let wt = format!("{}-wt", repo.path());
        let det = format!("{}-det", repo.path());
        repo.sh(&format!("git worktree add -q -b b {wt} && git worktree add -q --detach {det} && git -C {det} commit -q --allow-empty -m loose"));
        let probe = |dir: &str| -> (bool, String) {
            let out = std::process::Command::new("sh").arg("-c").arg(&script).current_dir(dir).output().unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let run = |dir: &str, sh: &str| assert!(std::process::Command::new("sh").arg("-c").arg(sh).current_dir(dir).status().unwrap().success());
        // Everything the probe looks at, years old: the reflog's entry times
        // and the changed files' mtimes.
        let backdate = |dir: &str| {
            run(
                dir,
                r#"h=$(git rev-parse --git-path logs/HEAD); if [ -f "$h" ]; then sed 's/> [0-9]* /> 1577836800 /' "$h" > "$h.new" && mv "$h.new" "$h"; fi; git ls-files -z -m -o --exclude-standard | xargs -0 touch -t 202001010000 2>/dev/null; true"#,
            )
        };

        assert_eq!(probe(&wt), (true, "touched 1\ncontained 1\non_default 1\n".into()), "just checked out");
        backdate(&wt);
        assert_eq!(probe(&wt).1, "touched 0\ncontained 1\non_default 1\n");
        // git gc rewrites every reflog (a fresh mtime); its entries stay old.
        run(&wt, "git reflog expire --all --expire=never && touch \"$(git rev-parse --git-path logs/HEAD)\"");
        assert_eq!(probe(&wt).1, "touched 0\ncontained 1\non_default 1\n", "a reflog rewritten by gc is not a touch");
        run(&wt, "rm \"$(git rev-parse --git-path logs/HEAD)\"");
        assert_eq!(probe(&wt), (true, "touched 0\ncontained 1\non_default 1\n".into()), "no reflog, nothing changed");
        // The first path after logs/HEAD: catches a list that isn't NUL-separated.
        std::fs::write(format!("{wt}/new.txt"), "x").unwrap();
        assert!(probe(&wt).1.starts_with("touched 1\n"), "a fresh untracked file");
        std::fs::remove_file(format!("{wt}/new.txt")).unwrap();
        std::fs::write(format!("{wt}/a.txt"), "edited").unwrap();
        assert!(probe(&wt).1.starts_with("touched 1\n"), "a fresh edit");
        backdate(&wt);
        assert!(probe(&wt).1.starts_with("touched 0\n"), "an old edit");

        assert_eq!(probe(&det).1, "touched 1\ncontained 0\non_default 0\n", "a commit on no ref");
        let (ok, out) = probe(&std::env::temp_dir().display().to_string());
        assert!(!ok && out.is_empty(), "outside a repo: fail, print nothing ({out})");
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&det);
    }

    /// prune-worktrees: the `git worktree prune` each pass starts with forgets
    /// a worktree whose directory is gone, never one that exists or that
    /// someone locked; a dead `git worktree add`'s "initializing" lock is
    /// lifted only once its directory is gone and the lock is old.
    #[test]
    fn prune_plugin_forgets_only_missing_worktrees() {
        let lua = mlua::Lua::new();
        let m = prune_module(&lua);
        let repo = crate::test_util::TestRepo::with_files(&[("a.txt", "a")]);
        let wt = |name: &str| format!("{}-{name}", repo.path());
        let add = |name: &str, setup: &str| {
            let (path, lock) = (wt(name), format!(".git/worktrees/$(basename {})/locked", wt(name)));
            repo.sh(&format!("git worktree add -q -b {name} {path} && {}", setup.replace("{path}", &path).replace("{lock}", &lock)));
        };
        let old = "touch -t 202001010000 {lock}";
        add("open", "true");
        add("gone", "rm -rf {path}");
        add("mine", &format!("git worktree lock {{path}} && {old} && rm -rf {{path}}"));
        add("fresh", "git worktree lock --reason initializing {path} && rm -rf {path}");
        add("stuck", &format!("git worktree lock --reason initializing {{path}} && {old} && rm -rf {{path}}"));
        add("busy", &format!("git worktree lock --reason initializing {{path}} && {old}"));
        repo.sh(&format!("git -c worktree.useRelativePaths=true worktree add -q -b relative {}", wt("relative")));
        repo.sh(&format!("git worktree lock --reason initializing {0} && touch -t 202001010000 .git/worktrees/$(basename {0})/locked && rm -rf {0}", wt("relative")));

        let script: String = m.get::<mlua::Function>("forget_missing_sh").unwrap().call(360).unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(&script).current_dir(repo.path()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        let mut unlocked: Vec<&str> = stdout.lines().filter_map(|l| l.strip_prefix("unlocked ")).filter_map(|p| p.rsplit('-').next()).collect();
        unlocked.sort();
        assert_eq!(unlocked, ["relative", "stuck"], "{stdout}");
        for name in ["gone", "stuck", "relative"] {
            assert!(stderr.contains(&format!("-{name}: ")), "{name} forgotten: {stderr}");
        }
        let list = std::process::Command::new("git").args(["-C", &repo.path(), "worktree", "list", "--porcelain"]).output().unwrap();
        // (git spells the temp dir /private/var on macOS: compare the ends)
        let mut left: Vec<String> = String::from_utf8_lossy(&list.stdout).lines().filter_map(|l| l.strip_prefix("worktree ")).filter_map(|p| p.rsplit('-').next()).map(String::from).collect();
        left[1..].sort();
        assert_eq!(left[1..], ["busy", "fresh", "mine", "open"], "{left:?}");
        assert!(std::path::Path::new(&format!("{}/a.txt", wt("open"))).exists(), "the open worktree's files stay");
        repo.sh("git rev-parse --verify -q refs/heads/gone >/dev/null && git rev-parse --verify -q refs/heads/stuck >/dev/null"); // branches stay
        // Run again: nothing more to do, and a repo without worktrees is fine.
        let again = std::process::Command::new("sh").arg("-c").arg(&script).current_dir(repo.path()).output().unwrap();
        assert!(again.status.success() && again.stdout.is_empty() && again.stderr.is_empty(), "{again:?}");
        let bare = crate::test_util::TestRepo::with_files(&[("a.txt", "a")]);
        let none = std::process::Command::new("sh").arg("-c").arg(&script).current_dir(bare.path()).output().unwrap();
        assert!(none.status.success() && none.stdout.is_empty(), "{none:?}");
        for name in ["open", "busy"] {
            let _ = std::fs::remove_dir_all(wt(name));
        }
    }

    /// dashboard plugin: parsing `ps`/`df`/`du` output (macOS and Linux) and
    /// turning it into sections.
    #[test]
    fn dashboard_plugin_measures_and_lays_out() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        lua.load("kitchen = { on = function() end, command = function() end, every = function() return {} end, config = {}, json = {}, dashboard = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/dashboard/init.lua");
        let m: mlua::Table = lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap();
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let parse: mlua::Function = m.get("parse").unwrap();
        let text = "mem 33554432 16777216\nproc 1048576 claude\nproc 2097152 Comet Helper (Renderer)\ndf 1000000000 400000000 /System/Volumes/Data\ndu 52428800 /home/dev/agi\ndu 1048576 /home/dev/air-worktrees\n";
        let p = to_json(parse.call(text).unwrap());
        assert_eq!(p["mem"], serde_json::json!({"total": 33554432, "used": 16777216}));
        assert_eq!(p["procs"][0]["name"], "Comet Helper (Renderer)", "biggest first, names with spaces");
        assert_eq!(p["df"]["mount"], "/System/Volumes/Data");
        assert_eq!(p["dirs"][1]["path"], "/home/dev/air-worktrees");

        let list = lua
            .to_value(&serde_json::json!([
                { "path": "/r/a", "role": "main" },
                { "path": "/r/a-x", "role": "edit", "tab": "t1" },
                { "path": "/r/a-y", "role": "edit" },
                { "path": "/r/a-kitchen-pr-12", "role": "edit" },
                { "path": "/r/a-kitchen-spare", "role": "spare" },
            ]))
            .unwrap();
        let counts = m.get::<mlua::Function>("worktree_counts").unwrap().call::<mlua::Value>(list).unwrap();
        assert_eq!(to_json(counts.clone()), serde_json::json!({"total": 5, "edit": 3, "edit_tabs": 1, "spare": 1}));

        let parsed: mlua::Value = parse.call(text).unwrap();
        let paths = lua.to_value(&serde_json::json!(["/home/dev/air-worktrees/ast-1"])).unwrap();
        let sections = to_json(m.get::<mlua::Function>("sections").unwrap().call((counts, parsed.clone(), parsed, paths, 1, 0)).unwrap());
        let titles: Vec<&str> = sections.as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
        assert_eq!(titles, ["Worktrees", "Disk", "RAM"]);
        assert_eq!(sections[0]["rows"][0], serde_json::json!({"label": "checkouts", "value": "3 (1 with tabs)"}));
        let disk = &sections[1]["rows"];
        assert_eq!((disk[0]["value"].as_str(), disk.as_array().unwrap().len()), (Some("381.5G / 953.7G"), 2), "the volume, then `top` = 1 folder");
        assert_eq!((disk[1]["label"].as_str(), disk[1]["mark"].as_bool()), (Some("agi"), Some(false)));
        assert_eq!(sections[2]["rows"][0]["value"], "16.0G / 32.0G");
        let none = to_json(m.get::<mlua::Function>("sections").unwrap().call((mlua::Value::Nil, mlua::Value::Nil, mlua::Value::Nil, mlua::Value::Nil, 5)).unwrap());
        assert_eq!((none[0]["title"].as_str(), none.as_array().unwrap().len()), (Some("Disk"), 1));
        let rows: crate::plugins::dashboard::Section = serde_json::from_value(none[0].clone()).unwrap();
        assert!(rows.rows.is_empty(), "an empty Lua table is an empty row list");
    }

    fn bazel_cache_module(lua: &mlua::Lua) -> mlua::Table {
        lua.load("kitchen = { on = function() end, command = function() end, every = function() return {} end, config = {}, json = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/bazel-cache/init.lua");
        lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap()
    }

    /// bazel-cache: reading a scan, and what a pass decides to stop and evict.
    #[test]
    fn bazel_cache_plugin_plans_evictions() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let m = bazel_cache_module(&lua);
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let val = |j: serde_json::Value| lua.to_value(&j).unwrap();
        const G: u64 = 1024 * 1024;
        let now = 1_000_000;

        let parse: mlua::Function = m.get("parse_scan").unwrap();
        let text = "df 104857600 52428800\n\
            server 9 /private/c/workspace/aa\n\
            server 10 /elsewhere/workspace/zz\n\
            base 1024 500 1 0 /c/workspace/aa\t/w/my tab\n\
            base 2048 400 0 1 /c/workspace/bb\t\n\
            size contents 300 /c/repo/contents/h/u 1\n\
            used contents 700 /c/repo/contents/h/u 1\n\
            size contents 50 /c/repo/contents/h/writing\n\
            size contents 50 /c/repo/contents/.trash/x\n\
            used contents 50 /c/repo/contents/.trash/x\n\
            size download 200 /c/repo/content_addressable/sha256/d\n\
            used download 600 /c/repo/content_addressable/sha256/d\n\
            pin aa h/u 1\n\
            pin aa h/v\n\
            disk 4096\n";
        let s = to_json(parse.call(text).unwrap());
        assert_eq!(s["df"], serde_json::json!({"size": 104857600, "avail": 52428800}));
        assert_eq!(s["bases"][0], serde_json::json!({"kib": 1024, "active": 500, "exists": true, "busy": false, "path": "/c/workspace/aa", "workspace": "/w/my tab", "pid": 9}), "server matched by folder name");
        assert_eq!((s["bases"][1]["pid"].clone(), s["bases"][1]["busy"].clone(), s["bases"][1]["workspace"].clone()), (serde_json::Value::Null, true.into(), serde_json::Value::Null));
        assert_eq!(
            s["entries"],
            serde_json::json!([
                { "kind": "contents", "kib": 300, "used": 700, "path": "/c/repo/contents/h/u 1" },
                { "kind": "download", "kib": 200, "used": 600, "path": "/c/repo/content_addressable/sha256/d" },
            ]),
            "no `used`: still being written; hidden folders: Bazel's trash"
        );
        assert_eq!(s["disk_kib"], 4096);
        assert_eq!(s["pins"], serde_json::json!({ "aa": { "h/u 1": true, "h/v": true } }));
        assert_eq!(to_json(parse.call("base 1 1 1 0 /x\t\n").unwrap()), serde_json::Value::Null, "no df: the scan failed");

        let plan: mlua::Function = m.get("plan").unwrap();
        let cfg = val(serde_json::json!({ "max_gb": 30, "target_gb": 20, "min_free_percent": 15, "idle_minutes": 30 }));
        let base = |path: &str, gib: u64, ago: u64, exists: bool, pid: Option<u64>, busy: bool| {
            let mut b = serde_json::json!({ "kib": gib * G, "active": now - ago, "exists": exists, "busy": busy, "path": path, "workspace": format!("/w{path}") });
            if let Some(p) = pid {
                b["pid"] = p.into();
            }
            b
        };
        let entry = |kind: &str, path: &str, gib: u64, ago: u64| serde_json::json!({ "kind": kind, "kib": gib * G, "used": now - ago, "path": path });
        let df = |size_g: u64, avail_g: u64| serde_json::json!({ "size": size_g * G, "avail": avail_g * G });
        let run = |scan: serde_json::Value| to_json(plan.call((val(scan), cfg.clone(), now)).unwrap());
        let paths = |p: &serde_json::Value| -> Vec<String> { p["evict"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap().to_string()).collect() };
        // (an empty Lua table comes back as {})
        let stop = |p: &serde_json::Value| -> Vec<u64> { p["stop"].as_array().map(|a| a.iter().map(|v| v.as_u64().unwrap()).collect()).unwrap_or_default() };

        // Under budget: only the orphan goes (not one a server still runs in, nor a kept worktree's).
        let p = run(serde_json::json!({
            "df": df(100, 50), "disk_kib": G,
            "bases": [base("/orphan", 1, 9000, false, None, false), base("/live-orphan", 1, 9000, false, Some(7), false), base("/kept", 2, 9000, true, None, false)],
            "entries": [entry("contents", "/c1", 1, 9000)], "pins": {},
        }));
        assert_eq!(paths(&p), ["/orphan"]);
        assert_eq!((p["over"].clone(), stop(&p), p["freed_kib"].clone(), p["total_kib"].clone()), (serde_json::Value::Null, vec![], G.into(), (6 * G).into()));

        // Over max_gb (42G): one least-recently-used list down to target_gb,
        // repo entries by last use and bases by last command, but nothing
        // used within idle_minutes. b-idle's server links to /h/c1.
        let over = |idle_ago: u64| {
            serde_json::json!({
                "df": df(1000, 500), "disk_kib": 2 * G,
                "bases": [base("/b-idle", 8, idle_ago, true, Some(9), false), base("/b-old", 10, 10000, true, None, false)],
                "entries": [entry("contents", "/h/c1", 6, 9000), entry("contents", "/h/c-recent", 6, 100), entry("download", "/d1", 6, 8000), entry("contents", "/h/c3", 4, 7000)],
                "pins": { "b-idle": { "h/c1": true, "h/c-recent": true } },
            })
        };
        let p = run(over(5000));
        assert_eq!(paths(&p), ["/b-old", "/h/c1", "/d1"]);
        assert_eq!((p["over"].as_str(), p["short_kib"].clone(), p["blocked"].clone()), (Some("size"), 0.into(), serde_json::Value::Null));
        assert_eq!(stop(&p), [9], "the idle server links to /h/c1: stopped first");
        let mut scan = over(5000);
        scan["pins"] = serde_json::json!({ "b-idle": { "h/c3": true } });
        assert_eq!(stop(&run(scan)), Vec::<u64>::new(), "it links to nothing the plan removes: left running");

        // A server ran a command 100 s ago: what it links to stays, everything
        // else (bases without a server, other repo entries) still goes; the
        // plan falls short.
        let p = run(over(100));
        assert_eq!(paths(&p), ["/b-old", "/d1", "/h/c3"]);
        assert_eq!((p["blocked"].as_str(), stop(&p), p["short_kib"].clone(), p["pinned_kib"].clone()), (Some("server busy in b-idle"), vec![], (2 * G).into(), (6 * G).into()));
        // A client holding a base keeps it and what it links to, even with no server.
        let mut scan = over(5000);
        scan["bases"][1]["busy"] = true.into();
        scan["pins"]["b-old"] = serde_json::json!({ "h/c3": true });
        let p = run(scan);
        assert_eq!(paths(&p), ["/h/c1", "/d1", "/b-idle"]);
        assert_eq!((p["blocked"].as_str(), stop(&p)), (Some("server busy in b-old"), vec![9]));

        // Low disk (10% free) with the caches under max_gb: free up to min_free_percent.
        let p = run(serde_json::json!({ "df": df(100, 10), "disk_kib": 0, "bases": [], "entries": [entry("download", "/d1", 6, 9000), entry("download", "/d2", 6, 8000)], "pins": {} }));
        assert_eq!((p["over"].as_str(), paths(&p)), (Some("disk"), vec!["/d1".to_string()]));

        let sp: mlua::Function = m.get("shell_path").unwrap();
        assert_eq!(sp.call::<String>("~/a b").unwrap(), r#""$HOME/a b""#);
        assert_eq!(sp.call::<String>("/x/$y\"").unwrap(), r#""/x/\$y\"""#);
    }

    /// bazel-cache: scan_sh and evict_sh on a fake cache tree, with fake
    /// Bazel servers and a client (`sh -c 'sleep 60; true' A-server.jar
    /// --output_base=…`: the pattern is in the args, as a real server's is).
    #[test]
    fn bazel_cache_scripts_on_a_fake_tree() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let m = bazel_cache_module(&lua);
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let root = std::env::temp_dir().join(format!("kt-bazel-{}", kitchen_core::util::new_id()));
        let r = root.display().to_string();
        let sh = |script: &str, stdin: &str| -> String {
            use std::io::Write;
            let mut c = std::process::Command::new("sh").arg("-c").arg(script).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
            c.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success(), "script failed");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        // A process whose args are `args`; reaped in the background so a kill leaves no zombie.
        let fake = |args: &[&str]| -> u32 {
            let mut c = std::process::Command::new("sh").arg("-c").arg("sleep 60; true").args(args).spawn().unwrap();
            let pid = c.id();
            std::thread::spawn(move || c.wait());
            pid
        };
        let client = fake(&["bazel-client"]);
        sh(
            &format!(
                "set -e; mkdir -p {r:?}; cd {r:?}; mkdir -p workspace/live workspace/kept workspace/orphan/execroot workspace/install repo/contents/h/u1 repo/contents/h/u2 \
                 repo/content_addressable/sha256/d disk; \
                 printf 'WORKSPACE: %s\\n\\nmore\\n' {r:?} > workspace/live/README; cp workspace/live/README workspace/kept/README; \
                 printf 'WORKSPACE: /nonexistent/wt\\n' > workspace/orphan/README; \
                 printf 'pid=999999\\nowner=client\\n' > workspace/live/lock; printf 'pid={client}\\nowner=client\\n' > workspace/kept/lock; \
                 touch -t 202001010000 workspace/live/lock; \
                 echo x > workspace/orphan/execroot/f; chmod 444 workspace/orphan/execroot/f; chmod 555 workspace/orphan/execroot; \
                 echo x > repo/contents/h/u1/f; touch -t 202001010000 repo/contents/h/u1.recorded_inputs; echo x > repo/contents/h/u2/f; \
                 mkdir repo/contents/h/u3; touch repo/contents/h/u3.recorded_inputs; \
                 echo x > repo/content_addressable/sha256/d/file; touch -t 202001010000 repo/content_addressable/sha256/d/file; echo x > disk/f; \
                 mkdir workspace/live/external workspace/kept/external workspace/orphan/external; \
                 ln -s {r:?}/repo/contents/h/u1 workspace/live/external/a; ln -s /elsewhere/x workspace/live/external/b; \
                 ln -s {r:?}/repo/contents/h/u2 workspace/kept/external/a; ln -s {r:?}/repo/contents/h/u3 workspace/orphan/external/a"
            ),
            "",
        );
        let server = fake(&["A-server.jar", &format!("--output_base={r}/workspace/live")]);
        // A server whose output base is spelled through a symlink (Bazel resolves them).
        sh(&format!("mkdir {r:?}/workspace/aliased && ln -s {r:?} {r:?}-alias"), "");
        let aliased = fake(&["A-server.jar", &format!("--output_base={r}-alias/workspace/aliased")]);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let scan_sh: String = m.get::<mlua::Function>("scan_sh").unwrap().call(r.as_str()).unwrap();
        let scan = m.get::<mlua::Function>("parse_scan").unwrap().call::<mlua::Value>(sh(&scan_sh, "")).unwrap();
        let s = to_json(scan);
        let bases: std::collections::BTreeMap<String, serde_json::Value> =
            s["bases"].as_array().unwrap().iter().map(|b| (b["path"].as_str().unwrap().rsplit('/').next().unwrap().to_string(), b.clone())).collect();
        assert_eq!(bases.keys().collect::<Vec<_>>(), ["aliased", "kept", "live", "orphan"], "not install/");
        assert_eq!(bases["aliased"]["pid"], aliased, "matched by folder name");
        assert_eq!(bases["live"]["pid"], server, "the fake server");
        assert_eq!((bases["live"]["exists"].as_bool(), bases["live"]["busy"].as_bool()), (Some(true), Some(false)), "lock's pid is gone");
        let in_2020 = |v: &serde_json::Value| (1577750000..1577930000).contains(&v.as_i64().unwrap());
        assert!(in_2020(&bases["live"]["active"]), "the backdated lock: {}", bases["live"]["active"]);
        assert_eq!(bases["kept"]["busy"], true, "a bazel client holds it");
        assert_eq!((bases["orphan"]["exists"].as_bool(), bases["orphan"]["workspace"].as_str()), (Some(false), Some("/nonexistent/wt")));
        let mut entries: Vec<(String, String, bool)> =
            s["entries"].as_array().unwrap().iter().map(|e| (e["kind"].as_str().unwrap().into(), e["path"].as_str().unwrap().rsplit('/').next().unwrap().into(), in_2020(&e["used"]))).collect();
        entries.sort();
        assert_eq!(entries, [("contents".to_string(), "u1".to_string(), true), ("contents".into(), "u3".into(), false), ("download".into(), "d".into(), true)], "u2 has no marker yet");
        assert_eq!(s["pins"], serde_json::json!({ "live": { "h/u1": true }, "kept": { "h/u2": true } }), "links of the server's and the client's bases, into repo/contents only");
        assert!(s["disk_kib"].as_u64().unwrap() > 0 && s["df"]["size"].as_u64().unwrap() > 0);

        let evict: mlua::Function = m.get("evict_sh").unwrap();
        let run = |plan: serde_json::Value| -> String {
            let (script, input): (String, String) = evict.call((lua.to_value(&plan).unwrap(), r.as_str())).unwrap();
            sh(&script, &input)
        };
        let item = |kind: &str, rel: &str| serde_json::json!({ "kind": kind, "path": format!("{r}/{rel}"), "kib": 1 });
        let out = run(serde_json::json!({ "stop": [], "evict": [item("base", "workspace/live"), item("base", "workspace/kept"), item("base", "workspace/orphan"), item("contents", "repo/contents/h/u1"), item("contents", "repo/contents/h/u3"), item("download", "repo/content_addressable/sha256/d")] }));
        assert_eq!(
            out,
            format!("skip {r}/workspace/live server\nskip {r}/workspace/kept client\nfreed {r}/workspace/orphan\nskip {r}/repo/contents/h/u1 linked\nskip {r}/repo/contents/h/u3 used\nfreed {r}/repo/content_addressable/sha256/d\n"),
            "a running server keeps what its output base links to, a fresh marker keeps its entry, the rest goes; read-only files too"
        );
        let out = run(serde_json::json!({ "stop": [], "evict": [item("base", "workspace/aliased")] }));
        assert_eq!(out, format!("skip {r}/workspace/aliased server\n"), "a server spelling root another way still counts");
        // Can't tell what's linked (here mktemp fails; a full disk would too): keep contents.
        let (script, input): (String, String) = evict.call((lua.to_value(&serde_json::json!({ "stop": [], "evict": [item("contents", "repo/contents/h/u1")] })).unwrap(), r.as_str())).unwrap();
        assert_eq!(sh(&format!("mktemp() {{ return 1; }}\n{script}"), &input), format!("skip {r}/repo/contents/h/u1 links unknown\n"));
        // Stopping the idle server frees what it linked to; a contents entry loses its marker too.
        let out = run(serde_json::json!({ "stop": [server], "evict": [item("contents", "repo/contents/h/u1"), item("base", "workspace/live")] }));
        assert_eq!(out, format!("freed {r}/repo/contents/h/u1\nfreed {r}/workspace/live\n"));
        assert!(!root.join("repo/contents/h/u1.recorded_inputs").exists() && root.join("repo/contents/h/u2").exists());
        let parsed = to_json(m.get::<mlua::Function>("parse_evict").unwrap().call("freed /a\nskip /b server\nstuck 4\n").unwrap());
        assert_eq!(parsed, serde_json::json!({ "freed": { "/a": true }, "skipped": ["/b server"], "stuck": 1 }));

        let _ = std::process::Command::new("kill").args([client.to_string(), aliased.to_string()]).status();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(format!("{r}-alias"));
    }

    fn mcp_token_sync_module(lua: &mlua::Lua) -> mlua::Table {
        lua.load("kitchen = { on = function() end, command = function() end, every = function() return {} end, config = {}, json = {} }").exec().unwrap();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/mcp-token-sync/init.lua");
        lua.load(std::fs::read_to_string(path).unwrap()).eval().unwrap()
    }

    /// mcp-token-sync: finding the mac's entries by serverName without their
    /// refresh tokens, hosts, payload, and reading the box's answers.
    #[test]
    fn mcp_token_sync_picks_entries_and_reads_answers() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        let m = mcp_token_sync_module(&lua);
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let val = |j: serde_json::Value| lua.to_value(&j).unwrap();
        let f = |name: &str| m.get::<mlua::Function>(name).unwrap();

        let slack = "plugin:slack:slack";
        let store = val(serde_json::json!({ "mcpOAuth": {
            "plugin:slack:slack|old": { "serverName": slack, "accessToken": "tok-old", "expiresAt": 1000, "refreshToken": "ref-old" },
            "plugin:slack:slack|live": {
                "serverName": slack, "accessToken": "tok-live", "expiresAt": 2_000_000_000_000i64, "refreshToken": "ref-live",
                "clientSecret": "sec", "clientId": "cid", "scope": "s", "discoveryState": { "oauthMetadataFound": true },
            },
            "plugin:slack:slack|noexp": { "serverName": slack, "accessToken": "tok-noexp" },
            "linear|h": { "serverName": "linear", "refreshToken": "ref-only" },
            "junk": "not an entry",
        }}));
        let names = val(serde_json::json!([slack, "linear", "gone"]));
        let got = to_json(f("mac_entries").call((store.clone(), names.clone())).unwrap());
        assert_eq!(
            got,
            serde_json::json!([
                {
                    "name": slack, "key": "plugin:slack:slack|live", "expires_at": 2_000_000_000_000i64,
                    "entry": { "serverName": slack, "accessToken": "tok-live", "expiresAt": 2_000_000_000_000i64, "clientId": "cid", "scope": "s", "discoveryState": { "oauthMetadataFound": true } },
                },
                { "name": "linear", "missing": true },
                { "name": "gone", "missing": true },
            ]),
            "the longest-lived entry, without refreshToken / clientSecret; no accessToken = missing"
        );
        let only_noexp = val(serde_json::json!({ "mcpOAuth": { "a|1": { "serverName": "a", "accessToken": "t" } } }));
        let got = to_json(f("mac_entries").call((only_noexp, val(serde_json::json!(["a"])))).unwrap());
        assert_eq!(got, serde_json::json!([{ "name": "a", "key": "a|1", "entry": { "serverName": "a", "accessToken": "t" } }]), "no expiresAt: picked, no expiry");
        let none = to_json(f("mac_entries").call((val(serde_json::json!({})), val(serde_json::json!(["a"])))).unwrap());
        assert_eq!(none, serde_json::json!([{ "name": "a", "missing": true }]), "no mcpOAuth at all");

        let now = 1_000_000.0;
        let left = |e: serde_json::Value| -> Option<f64> { f("seconds_left").call((val(e), now)).unwrap() };
        assert_eq!(left(serde_json::json!({ "expires_at": (1_000_000 + 301) * 1000 })), Some(301.0));
        assert_eq!(left(serde_json::json!({ "name": "a" })), None, "no expiry");
        assert_eq!(left(serde_json::json!({ "missing": true, "expires_at": 5 })), None);

        let workspaces = val(serde_json::json!([
            { "id": "w1", "host": "local" },
            { "id": "w2", "host": "box-a" },
            { "id": "w3", "host": "box-b" },
            { "id": "w4", "host": "box-a" },
        ]));
        let (hosts, ws_of): (mlua::Value, mlua::Value) = f("hosts").call(workspaces).unwrap();
        assert_eq!(to_json(hosts), serde_json::json!(["box-a", "box-b"]));
        assert_eq!(to_json(ws_of), serde_json::json!({ "box-a": ["w2", "w4"], "box-b": ["w3"] }));

        let entries = val(serde_json::json!([
            { "name": "live", "key": "live|1", "entry": { "accessToken": "t1" }, "expires_at": 2_000_000_000i64 },
            { "name": "expired", "key": "expired|1", "entry": { "accessToken": "t2" }, "expires_at": 999_000_000i64 },
            { "name": "forever", "key": "forever|1", "entry": { "accessToken": "t3" } },
            { "name": "gone", "missing": true },
        ]));
        let payload = to_json(f("payload").call((entries, now)).unwrap());
        assert_eq!(
            payload,
            serde_json::json!({ "servers": [
                { "name": "live", "key": "live|1", "entry": { "accessToken": "t1" } },
                { "name": "forever", "key": "forever|1", "entry": { "accessToken": "t3" } },
            ] }),
            "missing and expired entries stay home"
        );
        let entries = f("mac_entries").call::<mlua::Value>((store, names)).unwrap();
        let text = to_json(f("payload").call((entries, now)).unwrap()).to_string();
        assert!(!text.contains("refreshToken") && !text.contains("ref-") && !text.contains("sec"), "{text}");

        let status = |code: i32, out: &str, err: &str| -> String { f("box_status").call((code, out, err)).unwrap() };
        assert_eq!(status(0, "plugin:slack:slack:\n  Scope: x\n  Status: ✔ Connected\n  Type: http\n", ""), "connected");
        assert_eq!(status(0, "  Status: ✔ Connected", ""), "connected", "last line, no newline");
        assert_eq!(status(1, "", "No MCP server named \"x\". Configured servers: a"), "missing");
        assert_eq!(status(0, "  Status: ⚠ Needs authentication\n", ""), "⚠ Needs authentication");
        assert_eq!(status(127, "", "sh: claude: not found"), "unknown");
    }

    /// mcp-token-sync: MERGE_PY against a temp HOME — creates the file (600),
    /// keeps other keys, matches by serverName, writes only on change, never
    /// clobbers a broken file; then clears the server from the needs-auth cache.
    #[test]
    fn mcp_token_sync_merge_script_on_a_temp_home() {
        use std::io::Write;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let lua = mlua::Lua::new();
        let script: String = mcp_token_sync_module(&lua).get("MERGE_PY").unwrap();
        let home = std::env::temp_dir().join(format!("kt-mcp-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(&home).unwrap();
        let (dir, file) = (home.join(".claude"), home.join(".claude/.credentials.json"));
        let run = |payload: &serde_json::Value| -> (bool, String) {
            let mut c = std::process::Command::new("python3")
                .arg("-c")
                .arg(&script)
                .env("HOME", &home)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
            let out = c.wait_with_output().unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let read = || -> serde_json::Value { serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap() };
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let entry = serde_json::json!({ "serverName": "plugin:slack:slack", "accessToken": "tok-1", "expiresAt": 5 });
        let payload = serde_json::json!({ "servers": [{ "name": "plugin:slack:slack", "key": "plugin:slack:slack|mac", "entry": entry }] });

        assert_eq!(run(&payload), (true, "updated plugin:slack:slack\n".into()), "no ~/.claude yet");
        assert_eq!((mode(&dir), mode(&file)), (0o700, 0o600));
        assert_eq!(read(), serde_json::json!({ "mcpOAuth": { "plugin:slack:slack|mac": entry } }));
        let before = std::fs::metadata(&file).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(run(&payload), (true, String::new()), "unchanged: nothing printed");
        let after = std::fs::metadata(&file).unwrap();
        assert_eq!((after.ino(), after.mtime_nsec(), after.mtime()), (before.ino(), before.mtime_nsec(), before.mtime()), "unchanged: not rewritten");

        // The box hashed the server under another key; other keys and servers stay.
        let other = serde_json::json!({
            "claudeAiOauth": { "accessToken": "keep" },
            "mcpOAuth": {
                "plugin:slack:slack|box": { "serverName": "plugin:slack:slack", "accessToken": "stale" },
                "linear|h": { "serverName": "linear", "accessToken": "lin" },
                "weird": 3,
            },
        });
        std::fs::write(&file, other.to_string()).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(run(&payload), (true, "updated plugin:slack:slack\n".into()));
        assert_eq!(
            read(),
            serde_json::json!({
                "claudeAiOauth": { "accessToken": "keep" },
                "mcpOAuth": { "plugin:slack:slack|box": entry, "linear|h": { "serverName": "linear", "accessToken": "lin" }, "weird": 3 },
            }),
            "written under the box's key, not the mac's"
        );
        assert_eq!(mode(&file), 0o600, "a 644 file comes back 600");

        for broken in ["{not json", r#"{"mcpOAuth": null}"#, "[]", ""] {
            std::fs::write(&file, broken).unwrap();
            assert!(!run(&payload).0, "{broken:?}: fails");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), broken, "{broken:?}: left alone");
        }
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, [".credentials.json"], "no temp files left");

        // Claude's needs-auth cache: the pushed server's mark goes, others stay.
        let cache = dir.join("mcp-needs-auth-cache.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::write(&cache, serde_json::json!({ "plugin:slack:slack": { "timestamp": 1 }, "linear": { "timestamp": 2 } }).to_string()).unwrap();
        assert_eq!(run(&payload), (true, "updated plugin:slack:slack\ncleared plugin:slack:slack\n".into()));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&cache).unwrap()).unwrap(), serde_json::json!({ "linear": { "timestamp": 2 } }));
        std::fs::write(&cache, serde_json::json!({ "plugin:slack:slack": { "timestamp": 3 } }).to_string()).unwrap();
        assert_eq!(run(&payload), (true, "cleared plugin:slack:slack\n".into()), "cleared though the token is unchanged");
        let before = std::fs::metadata(&cache).unwrap();
        assert_eq!(run(&payload), (true, String::new()), "no mark: nothing printed");
        assert_eq!(std::fs::metadata(&cache).unwrap().ino(), before.ino(), "no mark: not rewritten");
        for broken in ["{not json", "[]", ""] {
            std::fs::write(&cache, broken).unwrap();
            assert_eq!(run(&payload), (true, String::new()), "{broken:?}: the token still goes");
            assert_eq!(std::fs::read_to_string(&cache).unwrap(), broken, "{broken:?}: left alone");
        }
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [".credentials.json", "mcp-needs-auth-cache.json"], "no temp files left");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `kitchen.every` runs on schedule and records its runs (stats, errors,
    /// SQLite); `t:run()` starts one now; a reload never overlaps a run that is
    /// still going. `kitchen.dashboard` sections follow the dashboard event.
    #[test]
    fn plugin_timers_and_dashboard() {
        let root = std::env::temp_dir().join(format!("kt-timers-{}", kitchen_core::util::new_id()));
        let dir = root.join("tick");
        std::fs::create_dir_all(&dir).unwrap();
        let out = root.join("slow.txt");
        std::fs::write(dir.join("plugin.toml"), format!("name = \"tick\"\napi = 6\n[config]\nout = {:?}\n", out.display().to_string())).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"
local n = 0
kitchen.every("fast", 0.05, function()
  n = n + 1
  if n == 1 then error("boom") end
  return { n = n }, { w1 = { here = n } }
end, { desc = "fast one", first_delay = 0 })
local function note(s)
  local f = io.open(kitchen.config.out, "a"); f:write(s .. "\n"); f:close()
end
local slow = kitchen.every("slow", 3600, function()
  note("start")
  kitchen.run_sync({ sh = "sleep 0.6" })
  note("end")
  return { ok = 1 }
end, { first_delay = 3600 })
kitchen.command("slow", function() slow:run() end)
kitchen.on("dashboard", function(ev)
  if ev.open then
    kitchen.dashboard.set(ev.workspace, { { title = "T", rows = { { label = "a", value = "1", fraction = 0.5 } } } })
  end
end)
"#,
        )
        .unwrap();
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![root.clone()]);
        let info = |name: &str| host.timers_info().into_iter().find(|t| t.name == name);
        wait(&|| info("fast").is_some_and(|t| t.state.runs >= 3 && t.state.last_error.is_none()));
        let fast = info("fast").unwrap();
        assert_eq!((fast.desc.as_str(), fast.interval_secs), ("fast one", 0.05));
        assert!(fast.state.stats["n"].as_i64().unwrap() >= 2 && fast.state.workspaces["w1"]["here"].is_number());
        assert!(engine.state().toasts.iter().any(|t| t.text.contains("boom")), "a failing run is reported");
        let saved = engine.db().timer_state("tick", "fast").unwrap().unwrap();
        assert!(serde_json::from_str::<TimerState>(&saved).unwrap().runs >= 2, "{saved}");

        // Start the slow one; while it runs, reload and ask the new load to run it again.
        let read = || std::fs::read_to_string(&out).unwrap_or_default();
        host.invoke("tick.slow", serde_json::json!({}), false).unwrap();
        wait(&|| read() == "start\n");
        host.reload(Some("tick")).unwrap();
        wait(&|| {
            let generation = lock(&host.slots)[0].generation;
            host.registry().commands.iter().any(|c| c.name == "tick.slow") && host.current("tick", generation)
        });
        host.invoke("tick.slow", serde_json::json!({}), false).unwrap();
        wait(&|| read() == "start\nend\n");
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(read(), "start\nend\n", "no second run while the first was going, none right after");
        assert_eq!(info("slow").unwrap().state.runs, 1);
        host.invoke("tick.slow", serde_json::json!({}), false).unwrap();
        wait(&|| read() == "start\nend\nstart\nend\n");
        wait(&|| info("slow").is_some_and(|t| t.state.runs == 2 && !t.running));

        // Dashboard sections arrive when the dashboard opens and go with the plugin.
        host.dashboard_shown("w1", true);
        wait(&|| host.dashboard.get("w1").is_some());
        assert_eq!(host.dashboard.get("w1").unwrap()[0].1[0].rows[0].fraction, Some(0.5));
        assert_eq!(host.dashboard.showing(), ["w1"]);
        host.reload(Some("tick")).unwrap();
        assert!(host.dashboard.get("w1").is_none(), "cleared on unload");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundled_plugins() {
        let repo_plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![repo_plugins]);
        wait(&|| host.list().len() == 11 && host.registry().viewer_maps.len() == 3 && host.registry().providers.len() == 2 && host.listeners("open") == ["github"]);
        assert!(host.has_provider("definition", "py") && !host.has_provider("definition", "rs"));
        for p in host.list() {
            assert_eq!(p.status, Status::Running, "{}", p.name);
        }
        let wt = std::env::temp_dir().join(format!("kt-bundled-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(&wt).unwrap();
        let text = "import os\n\ndef f():\n\t# TODO: é tidy\n\treturn undefined_name\n";
        let key = FileKey { host: "local".into(), worktree: wt.display().to_string(), path: "m.py".into() };
        host.file_loaded(key.clone(), Arc::from(text), false);
        let v = content_hash(text);
        wait(&|| host.decor.get(&key).is_some_and(|d| !d.highlights(&v).is_empty()));
        let d = host.decor.get(&key).unwrap();
        let h = &d.highlights(&v)[0];
        assert_eq!((h.line, h.col, h.end_col), (4, 4, Some(8)));
        // markdown renders .md files as they open, and binds ⌘⇧V.
        let md = FileKey { path: "README.md".into(), ..key.clone() };
        host.file_loaded(md.clone(), Arc::from("# Hi\n\ntext\n"), false);
        let mv = content_hash("# Hi\n\ntext\n");
        wait(&|| host.decor.get(&md).is_some_and(|d| d.render(&mv).is_some()));
        assert_eq!(host.decor.get(&md).unwrap().render(&mv).unwrap()[0].markdown, "# Hi\n\ntext");
        assert!(host.registry().chords.iter().any(|c| c.command == "markdown.toggle" && c.chord.label() == "⇧⌘V"));
        if std::process::Command::new("ruff").arg("--version").output().is_ok() {
            wait(&|| host.decor.get(&key).is_some_and(|d| !d.diagnostics(&v).is_empty()));
            let d = host.decor.get(&key).unwrap();
            let codes: Vec<(Option<&str>, usize, usize)> = d.diagnostics(&v).iter().map(|(_, d)| (d.code.as_deref(), d.line, d.col)).collect();
            assert!(codes.contains(&(Some("F401"), 1, 8)), "{codes:?}");
            assert!(codes.contains(&(Some("F821"), 5, 9)), "{codes:?}");
        }
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// ruff over ssh on a real devbox worktree (KT_VIEWER_HOST / KT_VIEWER_WT).
    #[test]
    #[ignore]
    fn devbox_ruff() {
        let (h, wt) = (std::env::var("KT_VIEWER_HOST").unwrap(), std::env::var("KT_VIEWER_WT").unwrap());
        let repo_plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![repo_plugins]);
        wait(&|| host.list().len() == 4 && host.registry().viewer_maps.len() == 3);
        let text = "import os\n\nprint(nope)\n";
        let key = FileKey { host: h, worktree: wt, path: "aster/kitchen_probe.py".into() };
        let start = Instant::now();
        host.file_loaded(key.clone(), Arc::from(text), false);
        wait(&|| host.decor.get(&key).is_some_and(|d| !d.diagnostics(&content_hash(text)).is_empty()));
        let d = host.decor.get(&key).unwrap();
        for (_, d) in d.diagnostics(&content_hash(text)) {
            eprintln!("{:?} {}:{} {}", d.code, d.line, d.col, d.message);
        }
        eprintln!("round trip {:?}", start.elapsed());
    }

    /// Ask `host` for `kind` at (line, col) of `key` and wait for the answer.
    fn ask(host: &Arc<PluginHost>, kind: &str, key: &FileKey, line: usize, col: usize) -> ProviderReply {
        let (tx, rx) = std::sync::mpsc::channel();
        let file = serde_json::json!({ "host": key.host, "worktree": key.worktree, "path": key.path, "abs": key.abs() });
        let ctx = serde_json::json!({ "viewer": { "path": key.path, "file": file, "line": line, "col": col } });
        assert!(host.provide(kind, &filetype_of(&key.path), ctx, move |r| {
            let _ = tx.send(r);
        }));
        rx.recv_timeout(Duration::from_secs(120)).expect("provider answered")
    }

    /// The bundled python plugin: ty (via uvx) answers gd / gr across files,
    /// including through a method call and into an installed package.
    #[test]
    fn python_lsp() {
        if std::process::Command::new("uvx").arg("--version").output().is_err() {
            eprintln!("uvx not installed; skipping");
            return;
        }
        let repo_plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![repo_plugins]);
        wait(&|| host.has_provider("references", "py"));
        let wt = std::env::temp_dir().join(format!("kt-pylsp-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(wt.join("proj/pkg")).unwrap();
        std::fs::write(wt.join("proj/pyproject.toml"), "[project]\nname = \"pkg\"\nversion = \"0\"\n").unwrap();
        std::fs::write(wt.join("proj/pkg/__init__.py"), "").unwrap();
        let a = "class Store:\n    def load(self, key: str) -> str:\n        return key\n\n\ndef make() -> Store:\n    return Store()\n";
        let b = "from pkg.a import make\nimport json\n\n\ndef run() -> str:\n\ts = make()\n\tjson.dumps(1)\n\treturn s.load(\"é\") + s.load(\"x\")\n";
        std::fs::write(wt.join("proj/pkg/a.py"), a).unwrap();
        std::fs::write(wt.join("proj/pkg/b.py"), b).unwrap();
        let wts = wt.display().to_string();
        let key = FileKey { host: "local".into(), worktree: wts.clone(), path: "proj/pkg/b.py".into() };
        host.file_loaded(key.clone(), Arc::from(b), false);

        // `s.load` after a tab and a non-ASCII string: raw columns, a method, another file.
        let defs = ask(&host, "definition", &key, 8, 26).expect("an answer");
        assert_eq!(defs, vec![Location { path: "proj/pkg/a.py".into(), line: 2, col: 9, text: None }], "{defs:?}");
        let defs = ask(&host, "definition", &key, 6, 6).expect("an answer");
        assert_eq!((defs[0].path.as_str(), defs[0].line), ("proj/pkg/a.py", 6));
        // Into the standard library: an absolute path outside the worktree.
        let defs = ask(&host, "definition", &key, 7, 8).expect("an answer");
        assert!(defs[0].path.starts_with('/') && defs[0].path.contains("json"), "{defs:?}");
        let refs = ask(&host, "references", &key, 8, 12).expect("an answer");
        let mut got: Vec<(String, usize, usize)> = refs.iter().map(|l| (l.path.clone(), l.line, l.col)).collect();
        got.sort();
        assert_eq!(got, vec![("proj/pkg/a.py".into(), 2, 9), ("proj/pkg/b.py".into(), 8, 11), ("proj/pkg/b.py".into(), 8, 25)], "{refs:?}");
        // Nothing to find: None, so the viewer falls back to its text search.
        assert_eq!(ask(&host, "definition", &key, 3, 1), None);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// gd / gr through ty over ssh on a real devbox (KT_VIEWER_HOST / KT_VIEWER_WT,
    /// a checkout of air): aster's rollout runner.
    #[test]
    #[ignore]
    fn devbox_python_lsp() {
        let (h, wt) = (std::env::var("KT_VIEWER_HOST").unwrap(), std::env::var("KT_VIEWER_WT").unwrap());
        let repo_plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![repo_plugins]);
        wait(&|| host.has_provider("references", "py"));
        let key = FileKey { host: h.clone(), worktree: wt, path: "aster/python/aster/worker/rollout_runner.py".into() };
        let text = kitchen_core::host::Host::parse(&h).read_file(&key.abs()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let find = |pat: &str| lines.iter().position(|l| l.contains(pat)).map(|i| (i + 1, lines[i].find(pat).unwrap() + 1)).unwrap();
        host.file_loaded(key.clone(), Arc::from(text.as_str()), false);
        let start = Instant::now();
        let (l, c) = find("_dataset_loader.load_task(");
        let defs = ask(&host, "definition", &key, l, c + "_dataset_loader.".len());
        eprintln!("load_task -> {defs:?} in {:?}", start.elapsed());
        assert!(defs.unwrap()[0].path.ends_with("dataset_loader.py"));
        let (l, c) = find("builder_for(");
        let start = Instant::now();
        let refs = ask(&host, "references", &key, l, c).unwrap();
        eprintln!("{} references to builder_for in {:?}", refs.len(), start.elapsed());
        assert!(refs.len() > 5, "{refs:?}");
        let (l, c) = find("from kubernetes import client");
        let defs = ask(&host, "definition", &key, l, c + "from kubernetes import ".len()).unwrap();
        eprintln!("kubernetes client -> {defs:?}");
        assert!(defs[0].path.contains("site-packages/kubernetes"), "{defs:?}");
    }

    /// A plugin end to end: commands, keymaps, diagnostics from a file
    /// event (via a job), reload replay, and budget/crash isolation.
    #[test]
    fn plugin_lifecycle() {
        let root = std::env::temp_dir().join(format!("kt-plugins-{}", kitchen_core::util::new_id()));
        let dir = root.join("demo");
        std::fs::create_dir_all(dir.join("lua/demo")).unwrap();
        std::fs::write(dir.join("plugin.toml"), "name = \"demo\"\nfiletypes = [\"py\"]\n[config]\nword = \"TODO\"\n").unwrap();
        std::fs::write(dir.join("lua/demo/util.lua"), "return { upper = function(s) return s:upper() end }\n").unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"
local util = require("demo.util")
kitchen.command("hello", function(ctx) kitchen.notify(util.upper("hi " .. (ctx.who or "?"))) end, { desc = "say hi" })
kitchen.keymap("cmd+shift+y", "demo.hello", { when = "viewer" })
kitchen.keymap("viewer", "<leader>h", "demo.hello")
local ok, err = pcall(kitchen.keymap, "cmd+w", "demo.hello")
assert(not ok and tostring(err):find("built%-in"), "built-in chords are refused: " .. tostring(err))
kitchen.command("spin", function() while true do end end)
kitchen.on("file_opened", function(ev)
  local text = ev.text()
  local items = {}
  local n = 0
  for line in (text .. "\n"):gmatch("(.-)\n") do
    n = n + 1
    local c = line:find(kitchen.config.word, 1, true)
    if c then table.insert(items, { line = n, col = c, end_col = c + 4, severity = "warning", message = "todo here" }) end
  end
  kitchen.run({ cmd = { "echo", "ran" } }, function(res)
    assert(res.stdout == "ran\n", res.stdout)
    kitchen.diagnostics.set(ev.file, items, { version = ev.version })
  end)
end)
"#,
        )
        .unwrap();
        let ctx = egui::Context::default();
        let engine = Engine::new(ctx, kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![root.clone()]);
        wait(&|| host.registry().viewer_maps.len() == 1);
        let reg = host.registry();
        assert_eq!(reg.commands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["demo.hello", "demo.spin"]);
        assert_eq!(reg.chords.len(), 1);
        assert_eq!(reg.viewer_maps[0].keys, "h");

        let key = FileKey { host: "local".into(), worktree: "/w".into(), path: "a.py".into() };
        host.file_loaded(key.clone(), Arc::from("x = 1\n# TODO: fix\n"), false);
        host.file_loaded(FileKey { path: "b.rs".into(), ..key.clone() }, Arc::from("// TODO"), false);
        wait(&|| host.decor.get(&key).is_some());
        let d = host.decor.get(&key).unwrap();
        let v = content_hash("x = 1\n# TODO: fix\n");
        let diags = d.diagnostics(&v);
        assert_eq!(diags.len(), 1);
        assert_eq!((diags[0].1.line, diags[0].1.col), (2, 3));
        assert!(host.decor.get(&FileKey { path: "b.rs".into(), ..key.clone() }).is_none(), "filetypes filter");

        // A runaway command hits the instruction budget; the plugin survives.
        host.invoke("demo.spin", serde_json::json!({}), false).unwrap();
        host.invoke("demo.hello", serde_json::json!({"who": "w"}), false).unwrap();
        wait(&|| engine.state().toasts.iter().any(|t| t.text.contains("HI W")));
        assert!(engine.state().toasts.iter().any(|t| t.text.contains("budget")));
        assert!(host.invoke("demo.hello", serde_json::json!({}), true).is_err(), "not remote-callable");

        // Reload: registrations and decorations are rebuilt from scratch.
        host.reload(Some("demo")).unwrap();
        wait(&|| host.decor.get(&key).is_some() && host.registry().commands.len() == 2);
        assert_eq!(host.list()[0].status, Status::Running);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `kitchen.link` rules land in the registry in order, a function target
    /// becomes a command that sees `ctx.link`, and a bad regex is an error.
    #[test]
    fn link_rules() {
        let root = std::env::temp_dir().join(format!("kt-plugins-{}", kitchen_core::util::new_id()));
        let dir = root.join("lk");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), "name = \"lk\"\n").unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"
kitchen.command("pr", function() end)
kitchen.link("AI-(\\d+)", function(ctx) kitchen.notify("ticket " .. ctx.link.groups[1] .. " " .. ctx.link.text) end)
kitchen.link("PR-\\d+", "pr")
local ok, err = pcall(kitchen.link, "(bad", "pr")
assert(not ok and tostring(err):find("bad regex"), tostring(err))
"#,
        )
        .unwrap();
        let engine = Engine::new(egui::Context::default(), kitchen_core::db::Db::open_in_memory().unwrap()).unwrap();
        let host = host_with(&engine, vec![root.clone()]);
        wait(&|| host.registry().links.len() == 2);
        assert_eq!(host.list()[0].status, Status::Running);
        let reg = host.registry();
        assert_eq!(reg.links.iter().map(|l| (l.regex.as_str(), l.command.as_str())).collect::<Vec<_>>(), vec![(r"AI-(\d+)", "lk.link1"), (r"PR-\d+", "lk.pr")]);
        host.invoke("lk.link1", serde_json::json!({ "link": { "text": "AI-7", "groups": ["7"] } }), false).unwrap();
        wait(&|| engine.state().toasts.iter().any(|t| t.text.contains("ticket 7 AI-7")));
        host.reload(Some("lk")).unwrap();
        wait(&|| host.registry().links.len() == 2);
        let _ = std::fs::remove_dir_all(&root);
    }
}
