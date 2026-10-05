//! What plugins put on the workspace dashboard (`kitchen.dashboard.set`):
//! sections of rows, per workspace and plugin. Shaped like
//! [`super::decor::DecorStore`]: the UI reads `Arc` snapshots.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::lock;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Row {
    pub label: String,
    #[serde(default)]
    pub value: String,
    /// Draw a bar this full (0..1).
    #[serde(default)]
    pub fraction: Option<f32>,
    /// Highlight the row (e.g. "this workspace's worktree").
    #[serde(default)]
    pub mark: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Section {
    pub title: String,
    /// Shown dimmed next to the title (e.g. "measured 15:20").
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default, deserialize_with = "rows")]
    pub rows: Vec<Row>,
}

/// Rows, where an empty Lua table (`{}`, which reaches us as a JSON object) is no rows.
fn rows<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Row>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(m) if m.is_empty() => Ok(Vec::new()),
        v => serde_json::from_value(v).map_err(serde::de::Error::custom),
    }
}

/// One workspace's sections, by plugin, in the order plugins first set them.
pub type WorkspaceSections = Vec<(String, Vec<Section>)>;

#[derive(Default)]
pub struct DashboardStore {
    by_ws: Mutex<HashMap<String, Arc<WorkspaceSections>>>,
    /// Workspaces whose dashboard is on screen (set by the UI).
    showing: Mutex<BTreeSet<String>>,
}

impl DashboardStore {
    /// Replace `plugin`'s sections on workspace `ws`.
    pub fn set(&self, plugin: &str, ws: &str, sections: Vec<Section>) {
        let mut by_ws = lock(&self.by_ws);
        let mut next: WorkspaceSections = by_ws.get(ws).map(|a| (**a).clone()).unwrap_or_default();
        match next.iter_mut().find(|(p, _)| p == plugin) {
            Some((_, s)) => *s = sections,
            None => next.push((plugin.to_string(), sections)),
        }
        by_ws.insert(ws.to_string(), Arc::new(next));
    }

    /// Drop `plugin`'s sections on `ws`, or on every workspace.
    pub fn clear(&self, plugin: &str, ws: Option<&str>) {
        let mut by_ws = lock(&self.by_ws);
        for (id, sections) in by_ws.iter_mut() {
            if ws.is_none_or(|w| w == id) && sections.iter().any(|(p, _)| p == plugin) {
                *sections = Arc::new(sections.iter().filter(|(p, _)| p != plugin).cloned().collect());
            }
        }
        by_ws.retain(|_, s| !s.is_empty());
    }

    /// The plugin unloaded.
    pub fn clear_owner(&self, plugin: &str) {
        self.clear(plugin, None);
    }

    pub fn get(&self, ws: &str) -> Option<Arc<WorkspaceSections>> {
        lock(&self.by_ws).get(ws).cloned()
    }

    /// Mark `ws`'s dashboard shown or hidden; true when that changed.
    pub fn set_showing(&self, ws: &str, on: bool) -> bool {
        let mut showing = lock(&self.showing);
        if on { showing.insert(ws.to_string()) } else { showing.remove(ws) }
    }

    pub fn showing(&self) -> Vec<String> {
        lock(&self.showing).iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(title: &str) -> Section {
        Section { title: title.into(), note: None, rows: vec![Row { label: "a".into(), value: "1".into(), fraction: Some(0.5), mark: false }] }
    }

    #[test]
    fn sections_per_plugin_and_workspace() {
        let d = DashboardStore::default();
        d.set("disk", "w1", vec![section("Disk")]);
        d.set("prune", "w1", vec![section("Prune")]);
        d.set("disk", "w1", vec![section("Disk 2")]);
        d.set("disk", "w2", vec![section("Disk")]);
        let w1 = d.get("w1").unwrap();
        assert_eq!(w1.iter().map(|(p, s)| (p.as_str(), s[0].title.as_str())).collect::<Vec<_>>(), [("disk", "Disk 2"), ("prune", "Prune")]);
        d.clear("disk", Some("w1"));
        assert_eq!(d.get("w1").unwrap().len(), 1);
        assert!(d.get("w2").is_some());
        d.clear_owner("disk");
        assert!(d.get("w2").is_none());
        assert!(d.set_showing("w1", true) && !d.set_showing("w1", true));
        assert_eq!(d.showing(), ["w1"]);
        assert!(d.set_showing("w1", false));
    }
}
