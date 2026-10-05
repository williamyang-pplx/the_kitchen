//! Plugins: neovim-style, hackable Lua (one state per plugin, each on its own
//! thread) plus external processes for heavy work. See docs/plugins-design.md.
//!
//! The UI never calls into Lua: it sends [`ToPlugin`] messages and reads
//! cached state (the [`Registry`] of commands/keymaps and the
//! [`decor::DecorStore`]). The host tracks everything a plugin registers, so
//! a reload (or crash) cleans up without the plugin's help.

pub mod decor;
pub mod jobs;
pub mod keys;
mod lua;
pub mod procs;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use decor::{DecorStore, FileKey};
use keys::Chord;

/// The plugin API level this build implements.
pub const API_LEVEL: u32 = 4;
/// Events the kitchen hands to the plugins that listen (`kitchen.on`):
/// `open` (text typed into ⌘I), `comment` (a viewer comment to post).
pub const EMITTED: &[&str] = &["open", "comment"];
/// File text above this is not handed to plugins (`ev.text()` returns nil).
pub const MAX_TEXT: usize = 2 * 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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
    /// Content hash of `text` (see [`content_version`]).
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

pub fn content_version(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
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
            registry: Mutex::new(Arc::new(Registry::default())),
            registry_gen: AtomicU64::new(0),
            open_files: Mutex::new(HashMap::new()),
            toasted: Mutex::new(HashSet::new()),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
        });
        let overrides = user_overrides();
        let enabled = |name: &str| overrides.get(name).and_then(|v| v.get("enabled")).and_then(|v| v.as_bool()) != Some(false);
        crate::viewer::highlight::set_plugin_dirs(host.discover().into_iter().filter(|(n, _)| enabled(n)).map(|(_, d)| d).collect());
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
        });
        self.decor.clear_owner(name);
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
        let version = content_version(&text);
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
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
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

    /// The plugins shipped in the repo load; `todo` highlights, and `ruff`
    /// (when installed) reports diagnostics with the right positions.
    #[test]
    fn github_plugin_turns_links_into_tab_specs() {
        use mlua::LuaSerdeExt;
        let lua = mlua::Lua::new();
        lua.load("kitchen = { on = function() end, command = function() end, config = {}, json = {} }").exec().unwrap();
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
        let spec: mlua::Function = m.get("spec").unwrap();
        let to_json = |v: mlua::Value| -> serde_json::Value { lua.from_value(v).unwrap() };
        let ws = lua.to_value(&serde_json::json!({"id": "w1", "default_branch": "main"})).unwrap();
        let open = lua.to_value(&serde_json::json!({"title": "Fix it", "baseRefName": "dev", "state": "OPEN"})).unwrap();
        let s = to_json(spec.call((ws.clone(), 12, open)).unwrap());
        assert_eq!(s["title"], "#12 Fix it");
        assert_eq!(s["rev"], "refs/kitchen/pr/12");
        assert_eq!(s["diff_base"], "origin/dev");
        assert_eq!(s["fetch"], serde_json::json!(["+refs/pull/12/head:refs/kitchen/pr/12", "+refs/heads/dev:refs/remotes/origin/dev"]));
        let merged = lua.to_value(&serde_json::json!({"title": "x", "baseRefName": "dev", "state": "MERGED", "mergeCommit": {"oid": "abc"}})).unwrap();
        assert_eq!(to_json(spec.call((ws.clone(), 12, merged)).unwrap())["diff_base"], "abc^1");
        let no_gh = to_json(spec.call((ws, 12, mlua::Value::Nil)).unwrap());
        assert_eq!((no_gh["title"].as_str(), no_gh["diff_base"].as_str()), (Some("PR #12"), Some("origin/main")));

        // Review comments: threads from gh's list, items for the viewer, post arguments.
        let pr_number: mlua::Function = m.get("pr_number").unwrap();
        assert_eq!(pr_number.call::<Option<u64>>("/r/air-kitchen-ro-pr-14211").unwrap(), Some(14211));
        assert_eq!(pr_number.call::<Option<u64>>("/r/air-kitchen-ro-main").unwrap(), None);
        let list = lua
            .to_value(&serde_json::json!([
                { "id": 2, "in_reply_to_id": 1, "path": "a.py", "user": "bo", "body": "agreed", "created_at": "2026-10-02T00:00:00Z" },
                { "id": 1, "path": "a.py", "line": 40, "side": "RIGHT", "user": "al", "body": "use a set", "created_at": "2026-10-01T00:00:00Z" },
                { "id": 3, "path": "a.py", "original_line": 7, "side": "LEFT", "user": "cy", "body": "old", "created_at": "2026-10-03T00:00:00Z" },
                { "id": 4, "path": "b.py", "line": 1, "user": "dee", "body": "x", "created_at": "2026-10-03T00:00:00Z" },
            ]))
            .unwrap();
        let threads: mlua::Value = m.get::<mlua::Function>("threads").unwrap().call(list).unwrap();
        let items = to_json(m.get::<mlua::Function>("items").unwrap().call((threads, "a.py")).unwrap());
        assert_eq!(items.as_array().unwrap().len(), 2, "the reply joins its thread; b.py is elsewhere");
        assert_eq!(items[0]["id"], "1");
        assert_eq!(items[0]["line"], 40);
        assert_eq!(items[0]["side"], "new");
        assert_eq!(items[0]["title"], "@al · L40 · 2026-10-01 · 1 reply");
        assert_eq!(items[0]["body"], "use a set\n\n↳ @bo: agreed");
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
        let v = content_version(text);
        wait(&|| host.decor.get(&key).is_some_and(|d| !d.highlights(&v).is_empty()));
        let d = host.decor.get(&key).unwrap();
        let h = &d.highlights(&v)[0];
        assert_eq!((h.line, h.col, h.end_col), (4, 4, Some(8)));
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
        wait(&|| host.decor.get(&key).is_some_and(|d| !d.diagnostics(&content_version(text)).is_empty()));
        let d = host.decor.get(&key).unwrap();
        for (_, d) in d.diagnostics(&content_version(text)) {
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
        let v = content_version("x = 1\n# TODO: fix\n");
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
}
