//! One plugin = one Lua 5.4 state on its own thread. The thread handles its
//! inbox (file events, command invocations, finished jobs) one message at a
//! time, so plugin code is single-threaded and never runs on the UI thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, Table, UserData, UserDataMethods, Value, Variadic, VmState};

use super::dashboard::Section;
use super::decor::{self, FileKey};
use super::jobs::{self, JobSpec};
use super::procs::{self, Proc, ProcSpec, Stream};
use super::keys::Chord;
use super::timers::{self, TimerSpec};
use super::{ChordEntry, CommandEntry, FileEvent, LinkEntry, Location, MAX_TEXT, PROVIDER_KINDS, PluginHost, ProviderEntry, ToPlugin, ViewerMap, When};
use crate::engine::UiCommand;

/// Lua instructions one callback may run before it is stopped.
const BUDGET: u64 = 500_000_000;
const HOOK_EVERY: u32 = 10_000;
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;

const LINT_LUA: &str = include_str!("lint.lua");
const LSP_LUA: &str = include_str!("lsp.lua");

static NEXT_CALLBACK: AtomicU64 = AtomicU64::new(1);

pub fn spawn(host: Arc<PluginHost>, name: String, dir: PathBuf, entry: String, config: serde_json::Value, generation: u64) -> Sender<ToPlugin> {
    let (tx, rx) = channel();
    let self_tx = tx.clone();
    let spawned = std::thread::Builder::new().name(format!("plugin-{name}")).spawn(move || {
        let h = host.clone();
        let n = name.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(h, &n, &dir, &entry, config, generation, rx, self_tx)));
        if let Err(panic) = result {
            let msg = panic.downcast_ref::<String>().cloned().or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "panic".into());
            host.report_error(&name, &format!("crashed: {msg}"));
            host.mark_failed(&name, generation, format!("crashed: {msg}"));
        }
        // Timer runs this load never finished (it stopped, crashed, or never got to them).
        host.timers_released(&name, generation);
    });
    if let Err(e) = spawned {
        log::error!("plugin thread: {e}");
    }
    tx
}

struct Ctx {
    host: Arc<PluginHost>,
    name: String,
    generation: u64,
    self_tx: Sender<ToPlugin>,
    /// Live `kitchen.spawn` processes (dropping one kills it).
    procs: RefCell<HashMap<u64, Proc>>,
}

#[allow(clippy::too_many_arguments)]
fn run(host: Arc<PluginHost>, name: &str, dir: &Path, entry: &str, config: serde_json::Value, generation: u64, rx: Receiver<ToPlugin>, self_tx: Sender<ToPlugin>) {
    let lua = Lua::new();
    let _ = lua.set_memory_limit(MEMORY_LIMIT);
    let used = Rc::new(Cell::new(0u64));
    {
        let used = used.clone();
        let _ = lua.set_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
            used.set(used.get() + HOOK_EVERY as u64);
            if used.get() > BUDGET {
                used.set(0);
                return Err(mlua::Error::runtime("instruction budget exceeded (runaway loop?) — the callback was stopped"));
            }
            Ok(VmState::Continue)
        });
    }
    let cx = Rc::new(Ctx { host: host.clone(), name: name.to_string(), generation, self_tx, procs: RefCell::new(HashMap::new()) });
    if let Err(e) = install_api(&lua, &cx, dir, config) {
        host.report_error(name, &format!("setup: {e}"));
        host.mark_failed(name, generation, format!("setup: {e}"));
        return;
    }
    let entry_path = dir.join(entry);
    if entry_path.is_file() {
        used.set(0);
        let loaded = std::fs::read_to_string(&entry_path).map_err(mlua::Error::external).and_then(|src| lua.load(src).set_name(format!("@{}", entry_path.display())).exec());
        if let Err(e) = loaded {
            host.report_error(name, &format!("{entry}: {e}"));
            host.mark_failed(name, generation, format!("{entry}: {e}"));
            return;
        }
    }
    host.log(name, "loaded");
    fire(&lua, &cx, &used, "startup", Value::Table(lua.create_table().expect("table")));

    while let Ok(msg) = rx.recv() {
        used.set(0);
        match msg {
            ToPlugin::Stop => break,
            ToPlugin::File(ev) => match event_table(&lua, &ev) {
                Ok(t) => fire(&lua, &cx, &used, if ev.changed { "file_changed" } else { "file_opened" }, Value::Table(t)),
                Err(e) => host.report_error(name, &format!("event: {e}")),
            },
            ToPlugin::Event { name: event, value } => match to_lua(&lua, &value) {
                Ok(v) => fire(&lua, &cx, &used, &event, v),
                Err(e) => host.report_error(name, &format!("{event}: {e}")),
            },
            ToPlugin::Invoke { command, ctx } => {
                let result = (|| -> mlua::Result<()> {
                    let commands: Table = state(&lua)?.get("commands")?;
                    let f: Option<Function> = commands.get(command.as_str())?;
                    let Some(f) = f else { return Err(mlua::Error::runtime(format!("{command} is not defined"))) };
                    f.call::<()>(to_lua(&lua, &ctx)?)
                })();
                if let Err(e) = result {
                    host.report_error(name, &format!("{command}: {e}"));
                }
            }
            ToPlugin::Provide { kind, request, ctx } => {
                let r = (|| -> mlua::Result<bool> {
                    let providers: Table = state(&lua)?.get("providers")?;
                    let Some(f) = providers.get::<Option<Function>>(kind.as_str())? else { return Ok(false) };
                    let h = host.clone();
                    let plugin = name.to_string();
                    let reply = lua.create_function(move |lua, v: Value| {
                        let answer = match v {
                            Value::Nil => None,
                            v => match serde_json::from_value::<Vec<Location>>(to_json(lua, v)?) {
                                Ok(locs) => Some(locs),
                                Err(e) => {
                                    h.report_error(&plugin, &format!("provider reply: expected {{ {{path, line, col}}, … }}: {e}"));
                                    None
                                }
                            },
                        };
                        h.reply(request, answer);
                        Ok(())
                    })?;
                    f.call::<()>((to_lua(&lua, &ctx)?, reply))?;
                    Ok(true)
                })();
                match r {
                    Ok(true) => {}
                    Ok(false) => host.reply(request, None),
                    Err(e) => {
                        host.report_error(name, &format!("{kind} provider: {e}"));
                        host.reply(request, None);
                    }
                }
            }
            ToPlugin::ProcOut { generation: g, proc, stream, data } => {
                if g != generation {
                    continue;
                }
                let r = (|| -> mlua::Result<()> {
                    let cbs: Option<Table> = state(&lua)?.get::<Table>("procs")?.get(proc)?;
                    let key = if stream == Stream::Stdout { "on_stdout" } else { "on_stderr" };
                    match cbs.and_then(|t| t.get::<Option<Function>>(key).ok().flatten()) {
                        Some(f) => f.call::<()>(lua.create_string(&data)?),
                        None if stream == Stream::Stderr => {
                            for line in String::from_utf8_lossy(&data).lines().filter(|l| !l.trim().is_empty()) {
                                host.log(name, &format!("[proc {proc}] {line}"));
                            }
                            Ok(())
                        }
                        None => Ok(()),
                    }
                })();
                if let Err(e) = r {
                    host.report_error(name, &format!("process output: {e}"));
                }
            }
            ToPlugin::ProcExit { generation: g, proc, code } => {
                if g != generation {
                    continue;
                }
                cx.procs.borrow_mut().remove(&proc);
                let r = (|| -> mlua::Result<()> {
                    let procs: Table = state(&lua)?.get("procs")?;
                    let cbs: Option<Table> = procs.get(proc)?;
                    procs.set(proc, Value::Nil)?;
                    if let Some(f) = cbs.and_then(|t| t.get::<Option<Function>>("on_exit").ok().flatten()) {
                        f.call::<()>(code)?;
                    }
                    Ok(())
                })();
                if let Err(e) = r {
                    host.report_error(name, &format!("process exit: {e}"));
                }
            }
            ToPlugin::Timer { generation: g, name: timer, run } => {
                let out = if g != generation {
                    Err("sent to an older load of the plugin".to_string())
                } else {
                    (|| -> mlua::Result<(serde_json::Value, serde_json::Value)> {
                        let timers: Table = state(&lua)?.get("timers")?;
                        let Some(f) = timers.get::<Option<Function>>(timer.as_str())? else {
                            return Err(mlua::Error::runtime(format!("timer {timer} is not defined")));
                        };
                        let (stats, per_ws): (Value, Value) = f.call(())?;
                        Ok((lua.from_value(stats)?, lua.from_value(per_ws)?))
                    })()
                    .map_err(|e| e.to_string())
                };
                if let Err(e) = &out {
                    host.report_error(name, &format!("timer {timer}: {e}"));
                }
                host.timer_done(name, &timer, run, out);
            }
            ToPlugin::JobDone { generation: g, callback, result } => {
                if g != generation {
                    continue; // from before a reload
                }
                let r = (|| -> mlua::Result<()> {
                    let callbacks: Table = state(&lua)?.get("callbacks")?;
                    let f: Option<Function> = callbacks.get(callback)?;
                    callbacks.set(callback, Value::Nil)?;
                    if let Some(f) = f {
                        f.call::<()>(to_lua(&lua, &result)?)?;
                    }
                    Ok(())
                })();
                if let Err(e) = r {
                    host.report_error(name, &format!("job callback: {e}"));
                }
            }
        }
    }
    cx.procs.borrow_mut().clear();
    host.log(name, "stopped");
}

/// Internal per-plugin tables: handlers, commands, job callbacks.
fn state(lua: &Lua) -> mlua::Result<Table> {
    lua.named_registry_value("kitchen_state")
}

fn fire(lua: &Lua, cx: &Ctx, used: &Cell<u64>, event: &str, arg: Value) {
    let handlers: Option<Table> = state(lua).and_then(|s| s.get::<Table>("handlers")).and_then(|h| h.get(event)).ok();
    let Some(handlers) = handlers else { return };
    for f in handlers.sequence_values::<Function>().flatten() {
        used.set(0);
        if let Err(e) = f.call::<()>(arg.clone()) {
            cx.host.report_error(&cx.name, &format!("on {event}: {e}"));
        }
    }
}

fn event_table(lua: &Lua, ev: &Arc<FileEvent>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("file", file_table(lua, &ev.key)?)?;
    t.set("filetype", ev.filetype.as_str())?;
    t.set("version", ev.version.as_str())?;
    t.set("changed", ev.changed)?;
    t.set("lines", ev.text.lines().count())?;
    let text = ev.clone();
    t.set("text", lua.create_function(move |_, ()| Ok((text.text.len() <= MAX_TEXT).then(|| text.text.to_string())))?)?;
    Ok(t)
}

fn file_table(lua: &Lua, k: &FileKey) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("host", k.host.as_str())?;
    t.set("worktree", k.worktree.as_str())?;
    t.set("path", k.path.as_str())?;
    t.set("abs", k.abs())?;
    Ok(t)
}

fn to_file(lua: &Lua, v: Value) -> mlua::Result<FileKey> {
    lua.from_value::<FileKey>(v).map_err(|e| mlua::Error::runtime(format!("expected a file table {{host, worktree, path}} (e.g. ev.file): {e}")))
}

/// Lua tables → JSON; an empty table becomes `[]`.
fn to_json(lua: &Lua, v: Value) -> mlua::Result<serde_json::Value> {
    let j: serde_json::Value = lua.from_value(v)?;
    Ok(match j {
        serde_json::Value::Object(m) if m.is_empty() => serde_json::Value::Array(Vec::new()),
        j => j,
    })
}

fn qualify(plugin: &str, name: &str) -> String {
    if name.starts_with(&format!("{plugin}.")) || name.contains('.') { name.to_string() } else { format!("{plugin}.{name}") }
}

fn argv_of(t: &Table, what: &str) -> mlua::Result<Vec<String>> {
    Ok(match (t.get::<Option<Vec<String>>>("cmd")?, t.get::<Option<String>>("sh")?) {
        (Some(v), _) => v,
        (None, Some(script)) => vec!["sh".into(), "-c".into(), script],
        (None, None) => return Err(mlua::Error::runtime(format!("{what} needs cmd = {{…}} or sh = \"…\""))),
    })
}

/// What `kitchen.spawn` returns: `p:write(s)`, `p:close()`, `p:kill()`, `p:alive()`, `p.id`.
struct ProcHandle {
    id: u64,
    cx: Rc<Ctx>,
}

impl UserData for ProcHandle {
    fn add_fields<F: mlua::UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("id", |_, this| Ok(this.id));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("write", |_, this, data: mlua::LuaString| {
            Ok(this.cx.procs.borrow().get(&this.id).is_some_and(|p| p.write(data.as_bytes().to_vec())))
        });
        methods.add_method("close", |_, this, ()| {
            if let Some(p) = this.cx.procs.borrow_mut().get_mut(&this.id) {
                p.close_stdin();
            }
            Ok(())
        });
        methods.add_method("kill", |_, this, ()| {
            if let Some(p) = this.cx.procs.borrow().get(&this.id) {
                p.kill();
            }
            Ok(())
        });
        methods.add_method("alive", |_, this, ()| Ok(this.cx.procs.borrow().contains_key(&this.id)));
    }
}

/// What `kitchen.every` returns: `t:run()` runs the timer now (false while a run is going).
struct TimerHandle {
    name: String,
    cx: Rc<Ctx>,
}

impl UserData for TimerHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("run", |_, this, ()| Ok(this.cx.host.fire_timer(&this.cx.name, &this.name)));
    }
}

fn job_spec(lua: &Lua, t: Table) -> mlua::Result<JobSpec> {
    let argv = argv_of(&t, "kitchen.run")?;
    let host: Option<String> = t.get("host")?;
    let timeout: Option<f64> = t.get("timeout")?;
    let _ = lua;
    Ok(JobSpec {
        host: kitchen_core::host::Host::parse(host.as_deref().unwrap_or("local")),
        argv,
        cwd: t.get("cwd")?,
        stdin: t.get::<Option<mlua::LuaString>>("stdin")?.map(|s| s.as_bytes().to_vec()),
        timeout: timeout.map(Duration::from_secs_f64).unwrap_or(jobs::DEFAULT_TIMEOUT),
    })
}

fn install_api(lua: &Lua, cx: &Rc<Ctx>, dir: &Path, config: serde_json::Value) -> mlua::Result<()> {
    let st = lua.create_table()?;
    st.set("handlers", lua.create_table()?)?;
    st.set("commands", lua.create_table()?)?;
    st.set("callbacks", lua.create_table()?)?;
    st.set("providers", lua.create_table()?)?;
    st.set("procs", lua.create_table()?)?;
    st.set("timers", lua.create_table()?)?;
    lua.set_named_registry_value("kitchen_state", st)?;

    // require() finds the plugin's lua/ dir, and the bundled kitchen.lint.
    let package: Table = lua.globals().get("package")?;
    let old: String = package.get("path")?;
    let d = dir.display();
    package.set("path", format!("{d}/lua/?.lua;{d}/lua/?/init.lua;{old}"))?;
    let preload: Table = package.get("preload")?;
    preload.set("kitchen.lint", lua.create_function(|lua, ()| lua.load(LINT_LUA).set_name("@kitchen/lint.lua").eval::<Value>())?)?;
    preload.set("kitchen.lsp", lua.create_function(|lua, ()| lua.load(LSP_LUA).set_name("@kitchen/lsp.lua").eval::<Value>())?)?;

    let k = lua.create_table()?;
    k.set("name", cx.name.as_str())?;
    k.set("api_level", super::API_LEVEL)?;
    k.set("config", to_lua(lua, &config)?)?;

    let c = cx.clone();
    k.set(
        "on",
        lua.create_function(move |lua, (event, f): (String, Function)| {
            if !matches!(event.as_str(), "startup" | "file_opened" | "file_changed") && !super::EMITTED.contains(&event.as_str()) {
                return Err(mlua::Error::runtime(format!("unknown event {event:?} (startup, file_opened, file_changed, {})", super::EMITTED.join(", "))));
            }
            if super::EMITTED.contains(&event.as_str()) {
                c.host.register_listener(&c.name, c.generation, &event);
            }
            let handlers: Table = state(lua)?.get("handlers")?;
            let list: Table = match handlers.get::<Option<Table>>(event.as_str())? {
                Some(t) => t,
                None => {
                    let t = lua.create_table()?;
                    handlers.set(event.as_str(), &t)?;
                    t
                }
            };
            list.push(f)
        })?,
    )?;

    let c = cx.clone();
    let command = lua.create_function(move |lua, (name, f, opts): (String, Function, Option<Table>)| {
        let full = qualify(&c.name, &name);
        let desc = opts.as_ref().and_then(|o| o.get::<Option<String>>("desc").ok().flatten()).unwrap_or_default();
        let remote = opts.as_ref().and_then(|o| o.get::<Option<bool>>("remote").ok().flatten()).unwrap_or(false);
        let commands: Table = state(lua)?.get("commands")?;
        commands.set(full.as_str(), f)?;
        c.host.register_command(&c.name, c.generation, CommandEntry { plugin: c.name.clone(), name: full.clone(), desc, remote });
        Ok(full)
    })?;
    k.set("command", command.clone())?;

    // kitchen.link(pattern, target): ⌘-clicking a match in a terminal runs
    // target (a command name, or a function given the command context plus
    // ctx.link = { text, groups }). The host matches; Lua runs on click only.
    let c = cx.clone();
    let link_command = command.clone();
    let link_anon = Cell::new(0u32);
    k.set(
        "link",
        lua.create_function(move |lua, (pattern, target): (String, Value)| {
            let regex = regex::Regex::new(&pattern).map_err(|e| mlua::Error::runtime(format!("kitchen.link: bad regex {pattern:?}: {e}")))?;
            let command = match target {
                Value::String(s) => qualify(&c.name, &s.to_str()?),
                Value::Function(f) => {
                    link_anon.set(link_anon.get() + 1);
                    let name = format!("{}.link{}", c.name, link_anon.get());
                    link_command.call::<String>((name, f, lua.create_table_from([("desc", pattern.clone())])?))?
                }
                _ => return Err(mlua::Error::runtime("link target must be a command name or a function")),
            };
            c.host.register_link(&c.name, c.generation, LinkEntry { plugin: c.name.clone(), regex, command });
            Ok(())
        })?,
    )?;

    let c = cx.clone();
    let anon = Rc::new(Cell::new(0u32));
    k.set(
        "keymap",
        lua.create_function(move |lua, (a, b, rest): (String, Value, Variadic<Value>)| {
            // A target is a command name or a function (wrapped in a command).
            let to_command = |target: Value, label: &str| -> mlua::Result<String> {
                match target {
                    Value::String(s) => Ok(qualify(&c.name, &s.to_str()?)),
                    Value::Function(f) => {
                        anon.set(anon.get() + 1);
                        let name = format!("{}.keymap{}", c.name, anon.get());
                        command.call::<String>((name, f, lua.create_table_from([("desc", label.to_string())])?))
                    }
                    _ => Err(mlua::Error::runtime("keymap target must be a command name or a function")),
                }
            };
            if a == "viewer" {
                let seq = match &b {
                    Value::String(s) => s.to_str()?.to_string(),
                    _ => return Err(mlua::Error::runtime("kitchen.keymap(\"viewer\", \"<leader>x\", target)")),
                };
                let keys = seq.strip_prefix("<leader>").or_else(|| seq.strip_prefix('\\')).ok_or_else(|| mlua::Error::runtime(format!("viewer maps start with <leader> (\\): {seq:?}")))?;
                if keys.is_empty() || keys.chars().count() > 3 {
                    return Err(mlua::Error::runtime("viewer maps are <leader> + 1–3 keys"));
                }
                let target = rest.into_iter().next().unwrap_or(Value::Nil);
                let command = to_command(target, &seq)?;
                c.host.register_viewer_map(&c.name, c.generation, ViewerMap { plugin: c.name.clone(), keys: keys.to_string(), command });
                return Ok(());
            }
            let chord = Chord::parse(&a).map_err(mlua::Error::runtime)?;
            let opts = match rest.into_iter().next() {
                Some(Value::Table(t)) => Some(t),
                _ => None,
            };
            let when = When::parse(&opts.as_ref().and_then(|o| o.get::<Option<String>>("when").ok().flatten()).unwrap_or_default()).map_err(|e| mlua::Error::runtime(e.to_string()))?;
            let filetype = opts.as_ref().and_then(|o| o.get::<Option<String>>("filetype").ok().flatten());
            let command = to_command(b, &a)?;
            c.host
                .register_chord(&c.name, c.generation, ChordEntry { plugin: c.name.clone(), chord, command, when, filetype })
                .map_err(|e| mlua::Error::runtime(e.to_string()))
        })?,
    )?;

    // Decorations: kitchen.diagnostics / kitchen.highlights / kitchen.comments.
    for kind in decor::Kind::ALL {
        let t = lua.create_table()?;
        let c = cx.clone();
        t.set(
            "set",
            lua.create_function(move |lua, (file, items, opts): (Value, Value, Option<Table>)| {
                let key = to_file(lua, file)?;
                let ns = namespace(&c.name, opts.as_ref());
                let version = opts.as_ref().and_then(|o| o.get::<Option<String>>("version").ok().flatten());
                let items = to_json(lua, items)?;
                match kind {
                    decor::Kind::Diagnostics => {
                        let items = decor::parse_diagnostics(items).map_err(mlua::Error::runtime)?;
                        c.host.decor.set_diagnostics(&key, &ns, items, version);
                    }
                    decor::Kind::Highlights => {
                        let items = decor::parse_highlights(items).map_err(mlua::Error::runtime)?;
                        c.host.decor.set_highlights(&key, &ns, items, version);
                    }
                    decor::Kind::Comments => {
                        let items = decor::parse_comments(items).map_err(mlua::Error::runtime)?;
                        c.host.decor.set_comments(&key, &ns, items);
                    }
                }
                repaint(&c);
                Ok(())
            })?,
        )?;
        let c = cx.clone();
        t.set(
            "clear",
            lua.create_function(move |lua, (file, opts): (Value, Option<Table>)| {
                let key = match file {
                    Value::Nil => None,
                    v => Some(to_file(lua, v)?),
                };
                c.host.decor.clear(key.as_ref(), &namespace(&c.name, opts.as_ref()), Some(kind));
                repaint(&c);
                Ok(())
            })?,
        )?;
        k.set(kind.name(), t)?;
    }

    // Processes.
    let c = cx.clone();
    k.set(
        "run",
        lua.create_function(move |lua, (spec, cb): (Table, Option<Function>)| {
            let spec = job_spec(lua, spec)?;
            let id = NEXT_CALLBACK.fetch_add(1, Ordering::Relaxed);
            if let Some(cb) = cb {
                let callbacks: Table = state(lua)?.get("callbacks")?;
                callbacks.set(id, cb)?;
            }
            let (tx, generation) = (c.self_tx.clone(), c.generation);
            std::thread::spawn(move || {
                let result = jobs::run(&spec);
                let _ = tx.send(ToPlugin::JobDone { generation, callback: id, result });
            });
            Ok(id)
        })?,
    )?;
    k.set("run_sync", lua.create_function(|lua, spec: Table| to_lua(lua, &jobs::run(&job_spec(lua, spec)?)))?)?;

    let c = cx.clone();
    k.set(
        "spawn",
        lua.create_function(move |lua, t: Table| {
            if c.procs.borrow().len() >= procs::MAX_PROCS {
                return Err(mlua::Error::runtime(format!("a plugin may run at most {} processes", procs::MAX_PROCS)));
            }
            let host: Option<String> = t.get("host")?;
            let spec = ProcSpec { host: kitchen_core::host::Host::parse(host.as_deref().unwrap_or("local")), argv: argv_of(&t, "kitchen.spawn")?, cwd: t.get("cwd")? };
            let id = NEXT_CALLBACK.fetch_add(1, Ordering::Relaxed);
            let cbs = lua.create_table()?;
            for key in ["on_stdout", "on_stderr", "on_exit"] {
                cbs.set(key, t.get::<Option<Function>>(key)?)?;
            }
            let p = Proc::spawn(&spec, id, c.generation, c.self_tx.clone()).map_err(mlua::Error::runtime)?;
            state(lua)?.get::<Table>("procs")?.set(id, cbs)?;
            c.procs.borrow_mut().insert(id, p);
            Ok(ProcHandle { id, cx: c.clone() })
        })?,
    )?;

    let c = cx.clone();
    k.set(
        "provide",
        lua.create_function(move |lua, (kind, f, opts): (String, Function, Option<Table>)| {
            if !PROVIDER_KINDS.contains(&kind.as_str()) {
                return Err(mlua::Error::runtime(format!("unknown provider {kind:?} ({})", PROVIDER_KINDS.join(", "))));
            }
            let filetypes: Vec<String> = opts.as_ref().and_then(|o| o.get::<Option<Vec<String>>>("filetypes").ok().flatten()).unwrap_or_default();
            state(lua)?.get::<Table>("providers")?.set(kind.as_str(), f)?;
            c.host.register_provider(&c.name, c.generation, ProviderEntry { plugin: c.name.clone(), kind, filetypes });
            Ok(())
        })?,
    )?;

    // Timers: kitchen.every(name, secs, fn, { desc, first_delay }) → t, t:run().
    let c = cx.clone();
    k.set(
        "every",
        lua.create_function(move |lua, (name, secs, f, opts): (String, f64, Function, Option<Table>)| {
            if !(secs.is_finite() && secs > 0.0) {
                return Err(mlua::Error::runtime(format!("kitchen.every: interval must be > 0 seconds, got {secs}")));
            }
            let desc = opts.as_ref().and_then(|o| o.get::<Option<String>>("desc").ok().flatten()).unwrap_or_default();
            let first_delay = opts.as_ref().and_then(|o| o.get::<Option<f64>>("first_delay").ok().flatten()).unwrap_or(secs).max(0.0);
            state(lua)?.get::<Table>("timers")?.set(name.as_str(), f)?;
            c.host.register_timer(TimerSpec {
                plugin: c.name.clone(),
                name: name.clone(),
                desc,
                interval: Duration::from_secs_f64(secs),
                first_delay: Duration::from_secs_f64(first_delay),
                generation: c.generation,
                registered_at: timers::wall_now(),
            });
            Ok(TimerHandle { name, cx: c.clone() })
        })?,
    )?;

    // Dashboard sections: kitchen.dashboard.set(ws, sections) / .clear(ws?) / .showing().
    let dash = lua.create_table()?;
    let c = cx.clone();
    dash.set(
        "set",
        lua.create_function(move |lua, (ws, sections): (String, Value)| {
            let sections: Vec<Section> = serde_json::from_value(to_json(lua, sections)?)
                .map_err(|e| mlua::Error::runtime(format!("kitchen.dashboard.set: expected {{ {{ title, note?, rows = {{ {{ label, value?, fraction?, mark? }} }} }} }}: {e}")))?;
            c.host.dashboard.set(&c.name, &ws, sections);
            repaint(&c);
            Ok(())
        })?,
    )?;
    let c = cx.clone();
    dash.set(
        "clear",
        lua.create_function(move |_, ws: Option<String>| {
            c.host.dashboard.clear(&c.name, ws.as_deref());
            repaint(&c);
            Ok(())
        })?,
    )?;
    let c = cx.clone();
    dash.set("showing", lua.create_function(move |_, ()| Ok(c.host.dashboard.showing()))?)?;
    k.set("dashboard", dash)?;

    // UI.
    let c = cx.clone();
    k.set(
        "notify",
        lua.create_function(move |_, (msg, level): (String, Option<String>)| {
            if let Some(e) = c.host.engine() {
                match level.as_deref() {
                    Some("error") | Some("warn") | Some("warning") => e.toast_err(format!("{}: {msg}", c.name)),
                    _ => e.toast(msg),
                }
            }
            Ok(())
        })?,
    )?;
    let c = cx.clone();
    k.set(
        "open",
        lua.create_function(move |_, (path, line): (String, Option<usize>)| {
            if let Some(e) = c.host.engine() {
                if let Some(tab_id) = e.selected_workspace().and_then(|w| w.selected().map(|t| t.row.id.clone())) {
                    e.push_ui(UiCommand::OpenViewer { tab_id, path: Some(path), line, diff: None });
                    e.ctx.request_repaint();
                }
            }
            Ok(())
        })?,
    )?;

    // Workspaces and their tabs, as in the `open` event.
    let c = cx.clone();
    k.set(
        "workspaces",
        lua.create_function(move |lua, ()| match c.host.engine() {
            Some(e) => to_lua(lua, &serde_json::to_value(e.workspace_info()).map_err(mlua::Error::external)?),
            None => Ok(Value::Table(lua.create_table()?)),
        })?,
    )?;

    // Tabs: a read-only tab at any git rev (see docs/plugins.md).
    let c = cx.clone();
    k.set(
        "tab_open",
        lua.create_function(move |lua, spec: Table| {
            #[derive(serde::Deserialize)]
            struct Spec {
                workspace: String,
                rev: String,
                name: Option<String>,
                #[serde(default)]
                fetch: Vec<String>,
                diff_base: Option<String>,
                title: Option<String>,
                focus: Option<bool>,
            }
            let s: Spec = serde_json::from_value(to_json(lua, Value::Table(spec))?)
                .map_err(|e| mlua::Error::runtime(format!("kitchen.tab_open: {e} (needs workspace, rev)")))?;
            let Some(e) = c.host.engine() else { return Ok(()) };
            let opts = crate::engine::NewTab {
                kind: Some("readonly".into()),
                rev: Some(s.rev),
                name: s.name,
                fetch: s.fetch,
                diff_base: s.diff_base,
                title: s.title,
                focus: s.focus,
                ..Default::default()
            };
            e.spawn("open tab", move |e| e.create_tab(&s.workspace, opts).map(|_| ()));
            Ok(())
        })?,
    )?;

    // Logging: kitchen.log and print go to ~/.kitchen/logs/plugins/<name>.log.
    let c = cx.clone();
    let log = lua.create_function(move |_, args: Variadic<Value>| {
        let parts: Vec<String> = args.iter().map(|v| v.to_string().unwrap_or_else(|_| format!("{v:?}"))).collect();
        c.host.log(&c.name, &parts.join("\t"));
        Ok(())
    })?;
    k.set("log", log.clone())?;
    lua.globals().set("print", log)?;
    k.set("log_path", PluginHost::log_path(&cx.name).display().to_string())?;

    let json = lua.create_table()?;
    json.set("encode", lua.create_function(|lua, v: Value| serde_json::to_string(&lua.from_value::<serde_json::Value>(v)?).map_err(mlua::Error::external))?)?;
    json.set(
        "decode",
        lua.create_function(|lua, s: String| {
            let v: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
            to_lua(lua, &v)
        })?,
    )?;
    json.set("null", lua.null())?;
    k.set("json", json)?;

    lua.globals().set("kitchen", k)?;
    Ok(())
}

/// Rust → Lua with JSON nulls as nil (not a `NULL` sentinel).
fn to_lua<T: serde::Serialize + ?Sized>(lua: &Lua, v: &T) -> mlua::Result<Value> {
    lua.to_value_with(v, mlua::serde::SerializeOptions::new().serialize_none_to_null(false).serialize_unit_to_null(false))
}

fn namespace(plugin: &str, opts: Option<&Table>) -> String {
    match opts.and_then(|o| o.get::<Option<String>>("ns").ok().flatten()) {
        Some(sub) if !sub.is_empty() => format!("{plugin}.{sub}"),
        _ => plugin.to_string(),
    }
}

fn repaint(c: &Ctx) {
    if let Some(e) = c.host.engine() {
        e.ctx.request_repaint();
    }
}
