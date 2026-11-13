//! The kitchen's keybindings: the bundled assets/keybindings.toml under the
//! user's ~/.kitchen/keybindings.toml, reloaded within a second of a change.
//! A section of the file is a place; what has focus picks a chain of places,
//! and the innermost place that binds a key wins.

pub mod app;
pub mod matcher;
pub mod notation;
pub mod places;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

pub use matcher::{Matcher, SeqTable, Step};
pub use notation::{Binding, MouseBinding, Press, pressed, presses, take};
pub use places::{ClickCmd, Cmd, EditCmd, FieldCmd, KeyFocus, ListCmd, MenuCmd, Place};

use crate::engine::Engine;
use crate::plugins::{Registry, lock};
use notation::{Click, Mods};
use places::{ActionDef, actions, def_of};

const DEFAULTS: &str = include_str!("../../assets/keybindings.toml");
const DEFAULT_LEADER: &str = "\\";
const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// Whose line a binding is: in one place, your line beats a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Default,
    User,
    Plugin,
    UserPlugin,
}

impl Origin {
    /// Lookup preference within a place (lower wins).
    fn rank(self) -> u8 {
        match self {
            Origin::UserPlugin => 0,
            Origin::User => 1,
            Origin::Default => 2,
            Origin::Plugin => 3,
        }
    }

    fn builtin(self) -> bool {
        matches!(self, Origin::Default | Origin::User)
    }
}

#[derive(Clone, Debug)]
struct Entry {
    binding: Binding,
    cmd: Cmd,
    origin: Origin,
    /// Plugin chords limited to one filetype (`kitchen.keymap {filetype=…}`).
    filetype: Option<String>,
}

/// The merged keymap. Immutable; a reload builds a new one.
#[derive(Clone, Debug)]
pub struct Keymap {
    leader: Vec<Press>,
    pub timeout: Duration,
    places: HashMap<Place, Vec<Entry>>,
    mouse: HashMap<Place, Vec<(MouseBinding, ClickCmd, Origin)>>,
    /// `[plugins]` lines, applied by `with_plugins`.
    user_plugins: BTreeMap<String, Vec<Binding>>,
    /// Per focus, the chain's sequences.
    seqs: HashMap<KeyFocus, SeqTable>,
}

/// One parsed keybindings.toml.
#[derive(Default)]
struct File {
    leader: Option<String>,
    timeout_ms: Option<u64>,
    sections: Vec<(Place, Vec<(&'static ActionDef, Vec<String>)>)>,
    plugins: Vec<(String, Vec<String>)>,
}

fn read_file(text: &str) -> Result<File, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        let line = e.span().map(|s| text[..s.start.min(text.len())].lines().count().max(1));
        let msg = e.message().lines().next().unwrap_or("").to_string();
        match line {
            Some(l) => format!("line {l}: {msg}"),
            None => msg,
        }
    })?;
    let mut f = File::default();
    for (k, v) in table {
        match (k.as_str(), v) {
            ("leader", toml::Value::String(s)) => f.leader = Some(s),
            ("timeout_ms", toml::Value::Integer(n)) if n > 0 => f.timeout_ms = Some(n as u64),
            ("leader", _) => return Err("leader: one key in quotes, like \"\\\\\" or \",\"".into()),
            ("timeout_ms", _) => return Err("timeout_ms: a number of milliseconds, like 2000".into()),
            ("plugins", toml::Value::Table(t)) => {
                for (name, keys) in t {
                    let at = format!("[plugins] {name:?}");
                    f.plugins.push((name, strings(keys, &at)?));
                }
            }
            (section, toml::Value::Table(t)) => {
                let place = Place::from_section(section).ok_or_else(|| format!("[{section}]: unknown section"))?;
                let mut rows = Vec::new();
                for (name, keys) in t {
                    let def = actions(place).iter().find(|d| d.name == name).ok_or_else(|| format!("[{section}] {name}: unknown action"))?;
                    rows.push((def, strings(keys, &format!("[{section}] {name}"))?));
                }
                f.sections.push((place, rows));
            }
            (other, _) => return Err(format!("{other}: unknown setting (sections are [global], [vim], …)")),
        }
    }
    Ok(f)
}

fn strings(v: toml::Value, at: &str) -> Result<Vec<String>, String> {
    let wrong = || format!("{at}: keys are a list of strings, like [\"cmd+t\"]");
    let toml::Value::Array(a) = v else { return Err(wrong()) };
    a.into_iter().map(|x| if let toml::Value::String(s) = x { Ok(s) } else { Err(wrong()) }).collect()
}

/// Log a note once per process (plugin merges repeat on every registry change).
fn note(msg: String) {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if lock(SEEN.get_or_init(Default::default)).insert(msg.clone()) {
        log::info!(target: "kitchen::keybindings", "{msg}");
    }
}

impl Keymap {
    /// The bundled defaults alone.
    pub fn defaults() -> Arc<Keymap> {
        static DEFAULT: OnceLock<Arc<Keymap>> = OnceLock::new();
        DEFAULT.get_or_init(|| Arc::new(Keymap::load(DEFAULTS, None).expect("bundled keybindings.toml").0)).clone()
    }

    /// The bundled defaults under `user` (a keybindings.toml's text).
    #[cfg(test)]
    pub fn with_user(user: &str) -> Keymap {
        Keymap::load(DEFAULTS, Some(user)).unwrap_or_else(|e| panic!("{e}")).0
    }

    /// Parse the defaults and the user's file and merge them. The error is
    /// one line naming the section, action or key at fault. The notes say
    /// which keys a user line took from a default.
    pub fn load(defaults: &str, user: Option<&str>) -> Result<(Keymap, Vec<String>), String> {
        let d = read_file(defaults).map_err(|e| format!("bundled defaults: {e}"))?;
        let u = match user {
            Some(t) => read_file(t)?,
            None => File::default(),
        };
        let leader_s = u.leader.clone().or(d.leader.clone()).unwrap_or_else(|| DEFAULT_LEADER.into());
        let leader = Binding::parse(&leader_s, &[]).map_err(|e| format!("leader: {e}"))?.0;
        if leader.len() != 1 {
            return Err(format!("leader {leader_s:?}: one key"));
        }
        let mut km = Keymap {
            leader,
            timeout: Duration::from_millis(u.timeout_ms.or(d.timeout_ms).unwrap_or(DEFAULT_TIMEOUT_MS)),
            places: HashMap::new(),
            mouse: HashMap::new(),
            user_plugins: BTreeMap::new(),
            seqs: HashMap::new(),
        };
        for (place, rows) in &d.sections {
            for (def, keys) in rows {
                km.add(*place, def, keys, Origin::Default)?;
            }
        }
        let mut notes = Vec::new();
        for (place, rows) in &u.sections {
            for (def, keys) in rows {
                km.places.entry(*place).or_default().retain(|e| e.cmd != def.cmd);
                if let Cmd::Click(c) = def.cmd {
                    km.mouse.entry(*place).or_default().retain(|(_, x, _)| *x != c);
                }
                km.add(*place, def, keys, Origin::User)?;
            }
        }
        km.settle_user_lines(&mut notes)?;
        km.validate()?;
        for (name, keys) in &u.plugins {
            let bindings = keys.iter().map(|k| Binding::parse(k, &km.leader).map_err(|e| format!("[plugins] {name:?}: {e}"))).collect::<Result<Vec<_>, _>>()?;
            km.user_plugins.insert(name.clone(), bindings);
        }
        km.build_seqs();
        Ok((km, notes))
    }

    fn add(&mut self, place: Place, def: &ActionDef, keys: &[String], origin: Origin) -> Result<(), String> {
        let at = |e: String| format!("[{}] {}: {e}", place.section(), def.name);
        for k in keys {
            if let Cmd::Click(c) = def.cmd {
                let b = MouseBinding::parse(k).map_err(at)?;
                self.mouse.entry(place).or_default().push((b, c, origin));
                continue;
            }
            let binding = Binding::parse(k, &self.leader).map_err(at)?;
            let list = self.places.entry(place).or_default();
            if !list.iter().any(|e| e.binding == binding && e.cmd == def.cmd) {
                list.push(Entry { binding, cmd: def.cmd.clone(), origin, filetype: None });
            }
        }
        Ok(())
    }

    /// In one place, two of your lines on one key are an error; your line
    /// on a default's key takes it.
    fn settle_user_lines(&mut self, notes: &mut Vec<String>) -> Result<(), String> {
        for (place, list) in self.places.iter_mut() {
            let name = |c: &Cmd| def_of(*place, c).map_or("?", |d| d.name);
            let users: Vec<(Binding, Cmd)> = list.iter().filter(|e| e.origin == Origin::User).map(|e| (e.binding.clone(), e.cmd.clone())).collect();
            for (i, (b, c)) in users.iter().enumerate() {
                if let Some((_, other)) = users[i + 1..].iter().find(|(b2, c2)| b2 == b && c2 != c) {
                    return Err(format!("[{}] {} and {} both bind {}", place.section(), name(c), name(other), b.label()));
                }
                let before = list.len();
                let taken: Vec<String> = list.iter().filter(|e| e.origin == Origin::Default && e.binding == *b).map(|e| name(&e.cmd).to_string()).collect();
                list.retain(|e| !(e.origin == Origin::Default && e.binding == *b));
                if list.len() < before {
                    notes.push(format!("[{}] {}: your {} takes it from {}", place.section(), b.label(), name(c), taken.join(", ")));
                }
            }
        }
        for (place, list) in self.mouse.iter_mut() {
            let users: Vec<MouseBinding> = list.iter().filter(|(_, _, o)| *o == Origin::User).map(|(b, ..)| *b).collect();
            for b in users {
                let before = list.len();
                list.retain(|(x, _, o)| !(*o == Origin::Default && *x == b));
                if list.len() < before {
                    notes.push(format!("[{}] {}: your line takes it from a default", place.section(), b.label()));
                }
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        for (place, list) in &self.places {
            for e in list {
                let at = || format!("[{}] {}", place.section(), def_of(*place, &e.cmd).map_or("?", |d| d.name));
                let label = e.binding.label();
                if matches!(e.cmd, Cmd::App(_) | Cmd::Plugin(_) | Cmd::Edit(_)) && !e.binding.is_chord() {
                    return Err(format!("{}: {label:?} needs cmd, alt or ctrl (plain keys belong to the terminal and the viewer)", at()));
                }
                if matches!(place, Place::Picker | Place::Menu | Place::Dialog | Place::Input) && e.binding.0.len() != 1 {
                    return Err(format!("{}: {label:?} must be one key", at()));
                }
                if *place == Place::Vim && matches!(e.binding.0.first(), Some(Press::Char('1'..='9'))) {
                    return Err(format!("{}: {label:?} starts with a count digit", at()));
                }
                if matches!(place, Place::Vim | Place::Viewer) && e.binding.0 == self.leader {
                    return Err(format!("leader {:?} is also {}", Binding(self.leader.clone()).label(), at()));
                }
            }
        }
        for (b, c, _) in self.mouse.get(&Place::Terminal).into_iter().flatten() {
            if *c == ClickCmd::OpenLink && b.mods == Mods::default() {
                return Err(format!("[terminal] open_link_click: {:?} needs a modifier (a plain click selects text)", b.label()));
            }
        }
        Ok(())
    }

    fn build_seqs(&mut self) {
        for list in self.places.values_mut() {
            list.sort_by_key(|e| e.origin.rank());
        }
        self.seqs.clear();
        for focus in KeyFocus::ALL {
            let mut v: Vec<(Binding, Cmd)> = Vec::new();
            for place in focus.chain() {
                for e in self.places.get(place).into_iter().flatten() {
                    let keep = match &e.cmd {
                        Cmd::App(_) | Cmd::Click(_) | Cmd::Edit(_) => false,
                        // Leader maps work in the code only.
                        Cmd::Plugin(_) => !e.binding.is_chord() && focus != KeyFocus::FileList,
                        _ => true,
                    };
                    if keep && !v.iter().any(|(b, _)| *b == e.binding) {
                        v.push((e.binding.clone(), e.cmd.clone()));
                    }
                }
            }
            self.seqs.insert(focus, SeqTable::new(v));
        }
    }

    /// The leader key.
    pub fn leader(&self) -> &[Press] {
        &self.leader
    }

    /// A plugin leader map's keys (`e` of `<leader>e`) after the leader.
    pub fn leader_map(&self, keys: &str) -> Option<Binding> {
        let rest = Binding::parse(keys, &[]).ok()?;
        Some(Binding(self.leader.iter().copied().chain(rest.0).collect()))
    }

    /// Every key of `cmd` in `place` (key and mouse bindings), as labels.
    pub fn keys(&self, place: Place, cmd: &Cmd) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let keys = self.places.get(&place).into_iter().flatten().filter(|e| e.cmd == *cmd).map(|e| e.binding.label());
        let clicks = self.mouse.get(&place).into_iter().flatten().filter(|(_, c, _)| Cmd::Click(*c) == *cmd).map(|(b, ..)| b.label());
        for k in keys.chain(clicks) {
            if !out.contains(&k) {
                out.push(k);
            }
        }
        out
    }

    /// The first `max` keys of `cmd`, spaced ("⌘T", "j  ↓"); "" when unbound,
    /// and callers drop the hint.
    pub fn hint(&self, place: Place, cmd: &Cmd, max: usize) -> String {
        self.keys(place, cmd).into_iter().take(max).collect::<Vec<_>>().join("  ")
    }

    /// The command `press` is bound to from `focus` outward: the first place
    /// in the chain that binds it; there, an entry for `filetype` beats a
    /// generic one.
    pub fn lookup(&self, focus: KeyFocus, filetype: Option<&str>, press: &Press) -> Option<&Cmd> {
        focus.chain().iter().find_map(|place| {
            self.places
                .get(place)
                .into_iter()
                .flatten()
                .filter(|e| e.binding.0.len() == 1 && e.binding.0[0] == *press)
                .filter(|e| e.filetype.as_deref().is_none_or(|f| Some(f) == filetype))
                .min_by_key(|e| (e.filetype.is_none(), e.origin.rank()))
                .map(|e| &e.cmd)
        })
    }

    /// Whether `press` is a ⌘ chord bound to an app action in any place.
    pub fn bound_anywhere(&self, press: &Press) -> bool {
        matches!(press, Press::Key { mods, .. } if mods.cmd)
            && self.places.values().flatten().any(|e| matches!(e.cmd, Cmd::App(_)) && e.binding.0.len() == 1 && e.binding.0[0] == *press)
    }

    /// Whether `press` alone is bound to anything outside [edit].
    pub(crate) fn binds(&self, press: &Press) -> bool {
        self.places.iter().filter(|(p, _)| **p != Place::Edit).flat_map(|(_, l)| l).any(|e| e.binding.0.len() == 1 && e.binding.0[0] == *press)
    }

    /// The [edit] command `press` is bound to.
    pub(crate) fn edit_cmd(&self, press: &Press) -> Option<EditCmd> {
        self.places.get(&Place::Edit).into_iter().flatten().find(|e| e.binding.0.len() == 1 && e.binding.0[0] == *press).and_then(|e| match e.cmd {
            Cmd::Edit(c) => Some(c),
            _ => None,
        })
    }

    /// The single presses bound to `cmd` in [edit].
    pub(crate) fn edit_presses(&self, cmd: EditCmd) -> Vec<Press> {
        self.places.get(&Place::Edit).into_iter().flatten().filter(|e| e.cmd == Cmd::Edit(cmd) && e.binding.0.len() == 1).map(|e| e.binding.0[0]).collect()
    }

    /// Mouse: is a `cmd` binding's modifier held (link hover).
    pub fn held(&self, place: Place, cmd: ClickCmd, m: egui::Modifiers) -> bool {
        self.mouse.get(&place).into_iter().flatten().any(|(b, c, _)| *c == cmd && b.button == Click::Primary && b.mods != Mods::default() && b.mods == Mods::of(m))
    }

    /// Mouse: did a `cmd` binding fire on `resp` this frame.
    pub fn clicked(&self, place: Place, cmd: ClickCmd, resp: &egui::Response, m: egui::Modifiers) -> bool {
        self.mouse.get(&place).into_iter().flatten().any(|(b, c, _)| *c == cmd && b.mods == Mods::of(m) && resp.clicked_by(b.button()))
    }

    /// The sequence table for a focus.
    pub fn sequences(&self, focus: KeyFocus) -> &SeqTable {
        &self.seqs[&focus]
    }

    /// The built-in binding a plugin default `b` would take: any non-plugin
    /// entry with that key in a place that shares a focus chain with
    /// `place`, or in [edit].
    pub fn builtin_conflict(&self, place: Place, b: &Binding) -> Option<String> {
        let shares = |p: Place| p == place || p == Place::Edit || KeyFocus::ALL.iter().any(|f| f.chain().contains(&place) && f.chain().contains(&p));
        Place::ALL.into_iter().filter(|p| shares(*p)).find_map(|p| {
            self.places.get(&p).into_iter().flatten().find(|e| e.origin.builtin() && e.binding == *b).map(|e| format!("[{}] {}", p.section(), def_of(p, &e.cmd).map_or("?", |d| d.name)))
        })
    }

    /// Same for a leader map's keys, against the viewer's sequences.
    pub fn sequence_conflict(&self, keys: &Binding) -> Option<String> {
        KeyFocus::Viewer.chain().iter().find_map(|p| {
            self.places.get(p).into_iter().flatten().find(|e| e.origin.builtin() && e.binding == *keys).map(|e| format!("[{}] {}", p.section(), def_of(*p, &e.cmd).map_or("?", |d| d.name)))
        })
    }

    fn push(&mut self, place: Place, e: Entry) {
        self.places.entry(place).or_default().push(e);
    }

    /// This keymap with the plugins' chords and leader maps merged. A
    /// `[plugins]` line replaces a command's plugin keys, in the places its
    /// registrations name (chords: their `when`; leader maps: the viewer).
    pub fn with_plugins(&self, reg: &Registry) -> Keymap {
        let mut km = self.clone();
        for c in reg.chords.iter().filter(|c| !self.user_plugins.contains_key(&c.command)) {
            km.push(c.when.place(), Entry { binding: c.chord.clone(), cmd: Cmd::Plugin(c.command.clone()), origin: Origin::Plugin, filetype: c.filetype.clone() });
        }
        for m in reg.viewer_maps.iter().filter(|m| !self.user_plugins.contains_key(&m.command)) {
            if let Some(binding) = self.leader_map(&m.keys) {
                km.push(Place::Viewer, Entry { binding, cmd: Cmd::Plugin(m.command.clone()), origin: Origin::Plugin, filetype: None });
            }
        }
        for (command, bindings) in &self.user_plugins {
            let mut places: Vec<(Place, Option<String>)> = reg.chords.iter().filter(|c| c.command == *command).map(|c| (c.when.place(), c.filetype.clone())).collect();
            if reg.viewer_maps.iter().any(|m| m.command == *command) {
                places.push((Place::Viewer, None));
            }
            places.dedup();
            if places.is_empty() {
                if reg.commands.iter().any(|c| c.name == *command) {
                    note(format!("[plugins] {command:?}: the plugin binds no keys for it, so there is no place to put yours"));
                }
                continue;
            }
            for (place, filetype) in places {
                for b in bindings {
                    if !b.is_chord() && matches!(place, Place::Global | Place::Terminal) {
                        note(format!("[plugins] {command:?}: {:?} is a sequence, and only chords work in [{}]", b.label(), place.section()));
                        continue;
                    }
                    let list = km.places.entry(place).or_default();
                    let taken: Vec<String> = list.iter().filter(|e| e.binding == *b && e.origin != Origin::UserPlugin).map(|e| def_of(place, &e.cmd).map_or_else(|| format!("{:?}", e.cmd), |d| d.name.to_string())).collect();
                    list.retain(|e| !(e.binding == *b && e.origin != Origin::UserPlugin));
                    if !taken.is_empty() {
                        note(format!("[plugins] {command:?} takes {} from [{}] {}", b.label(), place.section(), taken.join(", ")));
                    }
                    list.push(Entry { binding: b.clone(), cmd: Cmd::Plugin(command.clone()), origin: Origin::UserPlugin, filetype: filetype.clone() });
                }
            }
        }
        km.build_seqs();
        km
    }

    /// Plugin commands that have keys, with their labels, for ⌘/ and ⌘⇧P.
    pub fn plugin_keys(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for p in [Place::Global, Place::Terminal, Place::Viewer, Place::Plan] {
            for e in self.places.get(&p).into_iter().flatten() {
                if let Cmd::Plugin(c) = &e.cmd {
                    let keys = out.entry(c.clone()).or_default();
                    let l = e.binding.label();
                    if !keys.contains(&l) {
                        keys.push(l);
                    }
                }
            }
        }
        out
    }
}

/// The live keymap and what it was built from.
#[derive(Clone)]
struct Live {
    base: Arc<Keymap>,
    reg: Arc<Registry>,
    reg_gen: u64,
    effective: Arc<Keymap>,
    /// The user file's (length, mtime) when last read; None before the first read.
    stamp: Option<(u64, Option<SystemTime>)>,
}

impl Live {
    fn new() -> Live {
        let d = Keymap::defaults();
        Live { base: d.clone(), reg: Arc::default(), reg_gen: 0, effective: d, stamp: None }
    }
}

static LIVE: OnceLock<Mutex<Live>> = OnceLock::new();

fn live() -> &'static Mutex<Live> {
    LIVE.get_or_init(|| Mutex::new(Live::new()))
}

#[cfg(test)]
thread_local! {
    /// (base, effective) set by `with_keymap` on this thread.
    static TEST_KEYMAP: std::cell::RefCell<Option<(Arc<Keymap>, Arc<Keymap>)>> = const { std::cell::RefCell::new(None) };
}

/// The live keymap (base + plugins). In tests, the thread's `with_keymap`
/// keymap when one is set.
pub fn current() -> Arc<Keymap> {
    #[cfg(test)]
    if let Some(km) = TEST_KEYMAP.with(|t| t.borrow().as_ref().map(|(_, e)| e.clone())) {
        return km;
    }
    lock(live()).effective.clone()
}

/// The first key of `cmd` in `place` in the live keymap; "" when unbound.
pub fn hint(place: Place, cmd: impl Into<Cmd>) -> String {
    current().hint(place, &cmd.into(), 1)
}

/// "⌘M comment · ⌘⇧A ask": "{key} {what}" per hint from the live keymap,
/// unbound ones dropped.
pub fn hint_line(hints: &[(Place, Cmd, &str)]) -> String {
    let km = current();
    hints.iter().filter_map(|(p, c, what)| Some(km.hint(*p, c, 1)).filter(|k| !k.is_empty()).map(|k| format!("{k} {what}"))).collect::<Vec<_>>().join(" · ")
}

/// "{what} — {hint_line}", or just `what` when no hint has a key.
pub fn labelled(what: &str, hints: &[(Place, Cmd, &str)]) -> String {
    match hint_line(hints) {
        h if h.is_empty() => what.to_string(),
        h => format!("{what} — {h}"),
    }
}

/// Run `f` with `km` as this thread's `current()`. Plugin threads don't see
/// it; in tests `refresh_plugins` only acts on a thread with an override, so
/// parallel plugin hosts never leak chords into other tests.
#[cfg(test)]
pub fn with_keymap<R>(km: Keymap, f: impl FnOnce() -> R) -> R {
    let km = Arc::new(km);
    let prev = TEST_KEYMAP.with(|t| t.replace(Some((km.clone(), km))));
    let r = f();
    TEST_KEYMAP.with(|t| *t.borrow_mut() = prev);
    r
}

/// The plugin registry changed: rebuild the live keymap. `reg_gen` is
/// taken inside the registry lock, so a late older snapshot is ignored.
pub fn refresh_plugins(reg: Arc<Registry>, reg_gen: u64) {
    #[cfg(test)]
    {
        TEST_KEYMAP.with(|t| {
            let mut t = t.borrow_mut();
            if let Some((base, _)) = t.as_ref() {
                let eff = Arc::new(base.with_plugins(&reg));
                *t = Some((base.clone(), eff));
            }
        });
        let _ = reg_gen;
    }
    #[cfg(not(test))]
    loop {
        // LIVE is never held while the registry lock is taken, nor while merging.
        let base = {
            let l = lock(live());
            if reg_gen <= l.reg_gen {
                return;
            }
            l.base.clone()
        };
        let effective = Arc::new(base.with_plugins(&reg));
        let mut l = lock(live());
        if reg_gen <= l.reg_gen {
            return;
        }
        if Arc::ptr_eq(&l.base, &base) {
            l.reg = reg;
            l.reg_gen = reg_gen;
            l.effective = effective;
            return;
        }
    }
}

/// `state_dir()/keybindings.toml`.
pub fn path() -> PathBuf {
    kitchen_proto::state_dir().join("keybindings.toml")
}

fn stamp(path: &Path) -> (u64, Option<SystemTime>) {
    match std::fs::metadata(path) {
        Ok(m) => (m.len(), m.modified().ok()),
        Err(_) => (0, None),
    }
}

/// One poll: reload when the file's (length, mtime) changed. Some(err) to toast.
fn reload(path: &Path, live: &mut Live, last_err: &mut Option<String>) -> Option<String> {
    let st = stamp(path);
    if live.stamp == Some(st) {
        return None;
    }
    live.stamp = Some(st);
    let text = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return report(format!("cannot read it: {e}"), last_err),
    };
    match Keymap::load(DEFAULTS, text.as_deref()) {
        Ok((km, notes)) => {
            for n in notes {
                log::info!(target: "kitchen::keybindings", "{n}");
            }
            live.effective = Arc::new(km.with_plugins(&live.reg));
            live.base = Arc::new(km);
            *last_err = None;
            None
        }
        Err(e) => report(e, last_err),
    }
}

/// An error to toast, unless it is the one toasted last.
fn report(e: String, last_err: &mut Option<String>) -> Option<String> {
    if last_err.as_ref() == Some(&e) {
        return None;
    }
    *last_err = Some(e.clone());
    Some(e)
}

/// Load the user's file now, then poll it every second on a
/// "keybindings-watch" thread.
pub fn watch(engine: &Arc<Engine>) {
    let path = path();
    let mut last_err = None;
    poll(&path, engine, &mut last_err);
    let weak = Arc::downgrade(engine);
    std::thread::Builder::new()
        .name("keybindings-watch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(engine) = weak.upgrade() else { return };
                poll(&path, &engine, &mut last_err);
            }
        })
        .ok();
}

fn poll(path: &Path, engine: &Arc<Engine>, last_err: &mut Option<String>) {
    let mut copy = lock(live()).clone();
    let before = copy.base.clone();
    let err = reload(path, &mut copy, last_err);
    let changed = !Arc::ptr_eq(&before, &copy.base);
    {
        let mut l = lock(live());
        l.stamp = copy.stamp;
        if changed {
            if l.reg_gen != copy.reg_gen {
                // The plugins changed meanwhile: merge the newer registry.
                copy.effective = Arc::new(copy.base.with_plugins(&l.reg));
            }
            l.base = copy.base;
            l.effective = copy.effective;
        }
    }
    if let Some(e) = err {
        engine.toast_err(format!("keybindings.toml: {e}"));
    }
    if changed {
        if let Some(p) = engine.plugins() {
            p.recheck_chords();
        }
        engine.ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::app::Action;
    use crate::viewer::vim::VimCmd;
    use egui::Key;

    fn chord(s: &str) -> Press {
        Binding::parse(s, &[]).unwrap().0[0]
    }

    #[test]
    fn bundled_defaults_load_without_notes() {
        let (km, notes) = Keymap::load(DEFAULTS, None).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        for place in Place::ALL {
            for def in actions(place) {
                assert!(!km.keys(place, &def.cmd).is_empty(), "[{}] {} has no default key", place.section(), def.name);
            }
        }
        // Every name in the file is an action (read_file would have failed).
        let f = read_file(DEFAULTS).unwrap();
        let named: usize = f.sections.iter().map(|(_, rows)| rows.len()).sum();
        let all: usize = Place::ALL.iter().map(|p| actions(*p).len()).sum();
        assert_eq!(named, all);
    }

    #[test]
    fn a_user_line_replaces_and_takes_keys() {
        let (km, notes) = Keymap::load(DEFAULTS, Some("[terminal]\nnew_tab = [\"cmd+n\"]\n")).unwrap();
        assert_eq!(km.lookup(KeyFocus::Terminal, None, &chord("cmd+n")), Some(&Cmd::App(Action::NewTab)));
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &chord("cmd+n")), Some(&Cmd::App(Action::NewWorkspace)));
        assert_eq!(km.lookup(KeyFocus::Terminal, None, &chord("cmd+t")), None, "the line replaces ⌘T");
        assert_eq!(notes.len(), 0, "⌘N is [global]'s, not [terminal]'s: no default lost it");
        let (km, notes) = Keymap::load(DEFAULTS, Some("[global]\nquit = [\"cmd+n\"]\n")).unwrap();
        assert_eq!(km.lookup(KeyFocus::Terminal, None, &chord("cmd+n")), Some(&Cmd::App(Action::Quit)));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(km.keys(Place::Global, &Cmd::App(Action::NewWorkspace)).is_empty());
        let km = Keymap::with_user("[vim]\ndown = []\n");
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &Press::Char('j')), None);
        assert!(km.hint(Place::Vim, &Cmd::Vim(VimCmd::Down), 2).is_empty());
    }

    #[test]
    fn bad_files_are_rejected_with_the_offending_name() {
        for (file, names) in [
            ("[nope]\n", "[nope]"),
            ("[vim]\nfly = [\"x\"]\n", "fly"),
            ("[vim]\ndown = [\"x\"]\nup = [\"x\"]\n", "both bind"),
            ("[vim]\ndown = [\"5\"]\n", "count digit"),
            ("[terminal]\nnew_tab = [\"t\"]\n", "new_tab"),
            ("leader = \"j\"\n", "leader"),
            ("[terminal]\nopen_link_click = [\"click\"]\n", "open_link_click"),
            ("[global]\nquit = \"cmd+q\"\n", "quit"),
            ("[global]\nquit = [\"cmd+nope\"]\n", "nope"),
            ("[plugins]\n\"x.y\" = [\"<X-a>\"]\n", "x.y"),
            ("timeout_ms = \"slow\"\n", "timeout_ms"),
            ("[vim\n", "line 1"),
        ] {
            let e = Keymap::load(DEFAULTS, Some(file)).unwrap_err();
            assert!(e.contains(names) && !e.contains('\n'), "{file:?}: {e}");
        }
    }

    #[test]
    fn the_innermost_place_wins() {
        let km = Keymap::defaults();
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &chord("cmd+d")), Some(&Cmd::App(Action::ToggleDiff)));
        assert_eq!(km.lookup(KeyFocus::Terminal, None, &chord("cmd+d")), Some(&Cmd::App(Action::Dashboard)));
        assert_eq!(km.lookup(KeyFocus::FileList, None, &chord("cmd+shift+l")), Some(&Cmd::App(Action::FocusCode)));
        assert_eq!(km.lookup(KeyFocus::FileList, None, &Press::Char('j')), Some(&Cmd::List(ListCmd::Down)));
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &Press::Char('j')), Some(&Cmd::Vim(VimCmd::Down)));
        assert_eq!(km.lookup(KeyFocus::Dialog, None, &chord("cmd+n")), None, "dialogs take no global chords");
        let enter = Press::Key { key: Key::Enter, mods: Default::default() };
        assert_eq!(km.lookup(KeyFocus::ViewerInput, None, &enter), Some(&Cmd::Field(FieldCmd::Submit)));
    }

    #[test]
    fn plugin_keys_join_by_place_and_your_plugins_line_replaces_them() {
        use crate::plugins::{ChordEntry, CommandEntry, ViewerMap, When};
        let mut reg = Registry::default();
        reg.commands.push(CommandEntry { plugin: "p".into(), name: "p.x".into(), desc: String::new(), remote: false });
        reg.chords.push(ChordEntry { plugin: "p".into(), chord: Binding::parse("cmd+shift+y", &[]).unwrap(), command: "p.x".into(), when: When::Viewer, filetype: None });
        reg.viewer_maps.push(ViewerMap { plugin: "p".into(), keys: "e".into(), command: "p.e".into() });
        let km = Keymap::defaults().with_plugins(&reg);
        let y = chord("cmd+shift+y");
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &y), Some(&Cmd::Plugin("p.x".into())));
        assert_eq!(km.lookup(KeyFocus::Terminal, None, &y), None);
        assert_eq!(km.sequences(KeyFocus::Viewer).exact(&[Press::Char('\\'), Press::Char('e')]), Some(&Cmd::Plugin("p.e".into())));
        assert_eq!(km.sequences(KeyFocus::FileList).exact(&[Press::Char('\\'), Press::Char('e')]), None, "no leader maps in the list");

        let km = Keymap::with_user("[plugins]\n\"p.x\" = [\"cmd+shift+u\"]\n\"p.e\" = [\"<leader>q\"]\n").with_plugins(&reg);
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &y), None);
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &chord("cmd+shift+u")), Some(&Cmd::Plugin("p.x".into())));
        assert_eq!(km.sequences(KeyFocus::Viewer).exact(&[Press::Char('\\'), Press::Char('q')]), Some(&Cmd::Plugin("p.e".into())));
        assert_eq!(km.plugin_keys()["p.e"], vec!["\\q"]);
        // Your [plugins] line may take a built-in key.
        let km = Keymap::with_user("[plugins]\n\"p.x\" = [\"cmd+p\"]\n").with_plugins(&reg);
        assert_eq!(km.lookup(KeyFocus::Viewer, None, &chord("cmd+p")), Some(&Cmd::Plugin("p.x".into())));
    }

    #[test]
    fn plugin_defaults_may_not_take_built_in_keys() {
        let km = Keymap::defaults();
        let b = |s: &str| Binding::parse(s, &[]).unwrap();
        assert_eq!(km.builtin_conflict(Place::Viewer, &b("cmd+shift+s")).as_deref(), Some("[global] view_menu"));
        assert!(km.builtin_conflict(Place::Global, &b("cmd+w")).is_some(), "⌘W closes a tab");
        assert!(km.builtin_conflict(Place::Global, &b("cmd+c")).is_some(), "an edit key");
        assert!(km.builtin_conflict(Place::Viewer, &b("ctrl+d")).is_some(), "vim's ⌃D");
        assert!(km.builtin_conflict(Place::Terminal, &b("ctrl+d")).is_none());
        assert!(km.builtin_conflict(Place::Terminal, &b("cmd+shift+h")).is_none(), "the viewer's keys don't reach the terminal");
        assert!(km.builtin_conflict(Place::Global, &b("cmd+shift+u")).is_none());
        assert!(km.builtin_conflict(Place::Global, &b("alt+l")).is_none());
        assert_eq!(km.sequence_conflict(&b("gg")).as_deref(), Some("[vim] go_to_top"));
        assert!(km.sequence_conflict(&km.leader_map("e").unwrap()).is_none());
    }

    #[test]
    fn an_edited_file_is_picked_up_and_a_bad_one_kept_out() {
        let dir = std::env::temp_dir().join(format!("kt-keys-{}", kitchen_core::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keybindings.toml");
        let mut live = Live::new();
        let mut last_err = None;
        assert_eq!(reload(&path, &mut live, &mut last_err), None, "no file: the defaults");
        assert_eq!(live.effective.lookup(KeyFocus::Terminal, None, &chord("cmd+t")), Some(&Cmd::App(Action::NewTab)));

        std::fs::write(&path, "[terminal]\nnew_tab = [\"cmd+y\"]\n").unwrap();
        assert_eq!(reload(&path, &mut live, &mut last_err), None);
        assert_eq!(live.effective.lookup(KeyFocus::Terminal, None, &chord("cmd+y")), Some(&Cmd::App(Action::NewTab)));
        let good = live.base.clone();
        assert_eq!(reload(&path, &mut live, &mut last_err), None);
        assert!(Arc::ptr_eq(&good, &live.base), "unchanged: not reread");

        std::fs::write(&path, "[terminal]\nnew_tab = [\"t\"]\n# bad\n").unwrap();
        let e = reload(&path, &mut live, &mut last_err).expect("an error to toast");
        assert!(e.contains("new_tab"), "{e}");
        assert!(Arc::ptr_eq(&good, &live.base), "the previous keymap stays");
        std::fs::write(&path, "[terminal]\nnew_tab = [\"t\"]\n# bad, again\n").unwrap();
        assert_eq!(reload(&path, &mut live, &mut last_err), None, "the same error is toasted once");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
