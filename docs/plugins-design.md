# Kitchen plugins — design (final, v1)

Neovim-style plugins: arbitrary Lua the user hacks on, plus external
processes for heavy work (linters), without the egui frame loop ever waiting
on plugin code. Draft 1 was reviewed adversarially; the fixes are folded in
and called out as *(review)*.

## Research → decisions

Five surveys (neovim; VS Code/Zed/LSP/Helix/Kakoune; WezTerm/Kitty/Sublime/
Micro/Obsidian/Raycast; Rust runtimes; the kitchen's own code) agreed:

* The host owns rendering. Plugins push **data** into **namespaced,
  replaceable sets** (neovim namespaces, Sublime `add_regions(key)`, VS Code
  `DiagnosticCollection`). The host tracks every registration so unload/reload
  cleans up without plugin cooperation (Obsidian).
* Plugin code never runs on the UI thread; results are cached snapshots.
* Heavy work = external processes, run *where the code lives* (the devbox).
* The same API for in-process code and external programs (neovim msgpack API,
  kitty remote control) → the socket API gets `decor.*` / `plugin.*`.
* Results are version-stamped; stale ones are dropped.

Runtime: **mlua + vendored Lua 5.4**, one Lua state per plugin, each on its own
thread (isolation from hangs; `send` feature not needed). WASM deferred (edit →
recompile, can't use the devbox's ruff), dylibs rejected, other scripting
languages rejected (ecosystem). Cost: ~10 s build, ~0.5 MB.

## 1. On disk

```
~/.kitchen/plugins/<name>/        + any dir listed in settings.plugin_dirs
  plugin.toml                     manifest (required)
  init.lua                        entry (optional → data-only plugin)
  lua/**.lua                      require() path
  syntaxes/*.sublime-syntax       extra grammars (loaded at startup; restart to change)
  themes/*.tmTheme                color theme (first enabled one wins; startup)
~/.kitchen/plugins.toml           [<name>] enabled = false / config overrides
~/.kitchen/logs/plugins/<name>.log
```

```toml
name = "ruff"            # id: namespace for commands, keymaps, decorations
version = "0.1.0"
description = "Python diagnostics from ruff"
api = 1                  # host refuses plugins with api > its level (logged)
filetypes = ["py", "pyi"]  # file events only for these (empty = all)
[config]                 # defaults; ~/.kitchen/plugins.toml [ruff] overrides keys
select = "E,F,W"
```

The repo ships `plugins/{ruff,todo,hello}`; `install.sh` symlinks them into
`~/.kitchen/plugins/` when absent.

## 2. Threads, budgets, reload

```
UI thread ── Event/Invoke ──▶ plugin inbox (mpsc) ─▶ plugin thread (owns Lua)
   ▲                                                     │ kitchen.run → job thread
   └── Arc<FileDecor> snapshots ◀── DecorStore ◀─────────┘ (JobDone → inbox)
```

* **Budget** *(review)*: an instruction hook counts Lua instructions per
  callback (default 500 M ≈ seconds of pure Lua). Time spent blocked in host
  calls (`run_sync`) is not counted. Overrun → Lua error.
* **No lock is held while calling into Lua or `dispatch`**; host mutexes
  recover from poisoning so a plugin panic can't wedge the UI *(review)*.
  Panics in a plugin thread are caught → plugin marked failed.
* **Jobs** *(review)*: `timeout` (default 30 s) kills the child (`ssh` for
  devboxes). Each job carries the plugin's load generation; results from
  before a reload are dropped.
* **Reload**: host drops the state and everything it registered, then loads
  fresh and **replays `file_opened` for every file the viewers hold**
  *(review)*. Triggered by `kitchenctl plugin reload`, the palette, or the
  watcher: mtimes of `plugin.toml` + `**/*.lua` (symlinks followed) polled
  every second, reload once stable for 300 ms; after a load error, wait for
  the next change instead of retrying.
* `kitchen.api`-style self-calls are fire-and-forget (no self-deadlock).
* Errors: logged; a toast once per (plugin, message).

## 3. Files, versions, units

* **File identity** *(review)*: `{host, worktree, path}` — edit tabs have their
  own worktrees, so `(workspace, path)` is ambiguous. `abs = worktree/path`.
* **Version** *(review)*: a content hash (`blake3`, 16 hex) of the text the
  viewer holds. External tools can compute it from disk. A decoration set
  with a `version` is shown only while it matches the viewer's text;
  unversioned sets always show.
* **Freshness** *(review)*: the viewer re-`stat`s its current file (one round
  trip) when shown and every 3 s while visible; if size/mtime changed it
  re-reads, rehashes and emits `file_changed`.
* **Units** *(review)*: lines 1-based; columns 1-based **Unicode scalar
  offsets into the raw line** (before tab expansion), `end_col` exclusive,
  optional `end_line`. The host converts to display offsets and clamps.
  Removed diff lines never show decorations; outline/folder views show none.

## 4. Lua API (`kitchen.*`, level 1)

```lua
kitchen.on("file_opened" | "file_changed", function(ev) end)
  -- ev.file = {host, worktree, path, abs}; ev.filetype; ev.version; ev.lines;
  -- ev.text() → contents (lazy; nil above 2 MB)
kitchen.command("fix", fn(ctx), { desc = "…", remote = false })   -- "ruff.fix"
  -- ctx = {workspace, tab, host, worktree, viewer = {path, line, col, sel = {from, to}}}
kitchen.keymap("cmd+shift+y", "ruff.fix", { when = "viewer" })
  -- when: "any"|"terminal"|"viewer"|"plan"; optional filetype
kitchen.keymap("viewer", "<leader>f", "ruff.fix")          -- viewer normal mode, leader = "\"
kitchen.diagnostics.set(file, items, { version = ev.version, ns = "…" })
  -- item {line, col, end_line?, end_col?, severity = "error"|"warning"|"info"|"hint", message, code?}
kitchen.diagnostics.clear(file?)
kitchen.highlights.set(file, { {line, col, end_col, group} }, opts)   -- last set wins, no priorities
kitchen.highlights.clear(file?)
kitchen.run({ cmd = {…} | sh = "…", cwd, host, stdin, timeout }, fn(res))  -- res {code, stdout, stderr, timed_out}
kitchen.run_sync({…}) → res                                   -- blocks this plugin only
kitchen.notify(msg, level?)   kitchen.open(path, line?)   kitchen.log(…)/print(…)
kitchen.config   kitchen.json.encode/decode   kitchen.name
require("kitchen.lint").define { … }   -- declarative linter (debounce + supersession)
```

Highlight `group`s are TextMate scopes resolved through the viewer's syntect
theme (`keyword`, `string`, `comment`, `entity.name.function`, …) *(review)*,
or `#rrggbb`. Items per set are capped (10 k).

## 5. Keymaps *(review)*

* Global chords match **exact modifiers**. Order: built-in ⌥ → built-in ⌘⇧ →
  plugin chords → built-in ⌘ → digits. Registering a chord that equals or is
  shadowed by a built-in fails with an error in the plugin log.
* Ctrl-only chords are refused for `when = "any" | "terminal"` (shell keys).
* **⌘⇧P** (a built-in ⌘⇧ chord) opens the command palette: plugin commands,
  "diagnostics in this file", "reload plugins".
* Viewer normal-mode maps start with the leader `\` (neovim's default; no
  Space leader). `]d` / `[d` jump between diagnostics.

## 6. Rendering *(review)*

Decorations are **never baked into `Doc`**. Per file the viewer keeps an
overlay (display line → spans, underlines, sign, virtual text) rebuilt only
when the file's decoration generation or hash changes, and `CodeView` applies
it while painting visible rows. Diagnostics: severity sign in a gutter
column, underline, first message as dim text after the line, full message for
the cursor line in the status bar. The UI reads a per-file `Arc<FileDecor>`
only when an atomic generation changed.

## 7. Socket API (kitchenctl + external programs)

`plugin.list`, `plugin.reload {name?}`, `plugin.run {command}`,
`decor.set {ns, host, worktree, path, kind, items, version?}`, `decor.clear`.
*(review)* Requests arriving via the devbox relay are marked remote: their
namespaces are forced to `ext.<ns>` (so is every socket push, local too),
`plugin.reload` is local-only, `plugin.run` only runs commands declared
`remote = true`.

## 8. Cut from v1

Tab/workspace/notification events, generic `kitchen.api`, highlight
priorities, plugin panels/UI, buffer editing, LSP client (buildable later on
`kitchen.run`), installer/registry, sandboxing (plugins are the user's code).

## Level 2 (2026-09-28): processes and providers

Added so `gd` / `gr` can come from language servers without the core knowing
about LSP:

* `kitchen.spawn` — long-lived processes. stdout/stderr chunks arrive as
  inbox messages (`ProcOut` / `ProcExit`) on the plugin's thread, writes go
  through a writer thread (a child that stops reading can't block Lua), and
  dropping the handle kills the child, so a reload or crash leaves nothing
  running.
* `kitchen.provide(kind, fn)` — the viewer asks a provider with a request id;
  `reply` may come later. The host fails every pending request of a plugin
  that unloads or crashes, and the viewer times out after 20 s, so a
  provider can never wedge `gd` — it always falls back to text search.
* `kitchen.lsp` (bundled Lua, like `kitchen.lint`) — the LSP client: framing,
  initialize, didOpen/didChange, workspace/configuration, position encoding
  (asks for utf-32 so columns are characters; converts utf-16 otherwise).
* The Python support itself is a plugin (`plugins/python`, ty by default).

## Level 3 (2026-10-04): opening tabs

* `kitchen.tab_open{workspace, rev, fetch, diff_base, name, title}` — a
  read-only tab at any rev, in worktree `<repo>-kitchen-ro-<name>`. The core
  has one read-only path: the latest-main tab is the default spec. Fetch +
  checkout are one host round trip, skipped when HEAD is already at `rev`;
  a second open of the same name reuses (and re-syncs) the tab.
* `diff_base` is a tab column (NULL = `origin/<default>`), used by the viewer
  and exported as `KITCHEN_DIFF_BASE`, so it survives session re-creation.
* `kitchen.on("open", ev)` — free text from ⌘I / `tab.open` goes to every
  plugin with an `open` handler (a broadcast, not a provider: providers are
  filetype-keyed and first-match only). No handler → an error toast.
* GitHub knowledge lives only in `plugins/github`.

## Level 4 (2026-10-04): comments

* `kitchen.comments.set(file, items)` — a third decoration kind (next to
  diagnostics and highlights, same namespaces and socket `decor.set
  kind=comments`), drawn as blocks under their rows. `side` picks the diff
  side the line number belongs to.
* `kitchen.on("comment", ev)` — ⌘M/⌘Enter in the viewer, broadcast like
  `open` (one registry of listeners for emitted events). The core never
  stores comments: the plugin owns them (GitHub is the store for PRs).
* `kitchen.workspaces()` so file events can be tied to tabs.

