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

* **File identity** *(review)*: `{host, worktree, path}` — tabs have their
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
  tab at any rev, in worktree `<repo>-<name>` (level 7 added `branch`, and
  dropped read-only tabs). Fetch + checkout are one host round trip; a
  second open of the same name reuses (and moves forward) the tab.
* `diff_base` is a tab column (NULL = `origin/<default>`), used by the viewer
  and exported as `KITCHEN_DIFF_BASE`, so it survives session re-creation.
* `kitchen.on("open", ev)` — a link from ⌘T / `tab.open` goes to every
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

## Level 5 (2026-10-05): terminal links

* ⌘-click in a terminal opens what is under the pointer. ⌘ because Claude
  Code turns mouse reporting on, so a plain click belongs to the program
  (Shift already bypasses reporting for selection).
* Matching is Rust (`links.rs`) over the logical line under the pointer
  (wrapped rows joined), only while ⌘ is held: built-ins for URLs and file
  paths, plus OSC 8 hyperlinks straight from the cells.
* `kitchen.link(pattern, target)` — plugins give a regex once; the host
  compiles and matches it, and Lua runs only on a click (through the
  command machinery, so `target` is a command name or a function, like
  `kitchen.keymap`). Running every plugin's matcher in Lua on hover would put
  a thread round trip in front of every frame.
* Files go through `UiCommand::OpenViewer` (jump list included), after one
  existence check on the tab's host so a misread path toasts instead of
  stealing focus. Relative paths resolve against the worktree.

## Level 8 (2026-10-06): PR review as a view

* `kitchen.view{tab, rev, name, fetch, base}` replaces `kitchen.tab_open`: a
  PR is checked out detached beside the repo and shown in the current tab's
  viewer, with no tab, tmux session or agent of its own. `diff_base` is no
  longer a tab column; `open` comes from ⌘⇧P with the tab's command ctx.

## Level 11 (2026-11-11): the PR menu

* The tab's agent lists its PRs in `.kitchen/tabs/<tab>/prs`
  (`$KITCHEN_PRS_FILE`), next to its plan. A tab can hold several stacks, and
  no git history says reliably which branches a tab made (a reused spare
  brings its reflog along; `git branch` leaves no trace of the worktree).
* `views` event + `kitchen.views{tab, entries}`: the core owns ⌘⇧S, the menu
  (a picker in menu mode: sections, j/k, numbered rows), the header button and the
  viewer cache. Plugins only say what the rows are; GitHub knowledge stays in
  the github plugin. The core reads the list and the checked-out branch in
  one round trip and hands both over, so plugins don't need the tab dir.
* Rows resolve to (worktree, base) in the engine (`shown_by`, shared with
  `kitchen.view`), so the menu and the header know which row is on screen.
* The app keeps one viewer per (tab, worktree, base) until the tab closes.
  Every switch takes a number when it is picked, and the UI drops a
  `ShowViewer` older than the last switch, so a slow first checkout can't
  take the screen back. A cached row shows at once; its checkout is then
  refreshed, and its viewers are rebuilt (same file and line) only if HEAD
  moved.
* Speed (2026-11-12). On a devbox's agi (a `blob:none` partial clone) every
  git command was slow because every on-demand download leaves a pack, and
  nothing merges them: 55k packs, an object lookup 395 ms, the viewer's diff
  listing 896 ms. `Repo::index_packs` writes a multi-pack index (lookup
  89 ms, listing 155 ms) at workspace setup, and the spare warmer rewrites it
  once 50 packs are newer than it; it never repacks, so promisor packs stay
  as they are. And a view's `have` skips its fetch (4 s) when GitHub's head
  and base commits are already here. Git already fetches a diff's or a
  checkout's missing contents in one batch, so the kitchen doesn't.

## Level 12 (2026-11-12): Markdown as GitHub renders it

* Markdown in the viewer (comment cards, ⌘⇧A answers, plan comments, `.md`
  files under ⌘⇧V, ⌘⇧D's description) is rendered the way GitHub and cmux
  render it: HTML, GitHub's CSS (github-markdown-css, vendored), WebKit.
  Drawing it with egui widgets (egui_commonmark) could not look like GitHub:
  spacing, list markers and bold all differed.
* The HTML is GitHub's own when there is a repo (`gh api markdown`, gfm, the
  repo as context), cached in memory and in `~/.kitchen/cache/markdown/`.
  comrak renders it meanwhile, and when GitHub can't (offline, no gh, no
  repo), with the viewer's syntax colors, sanitized by ammonia.
* WebKit lays the HTML out in a few WKWebViews parked outside the window,
  and their snapshots become egui textures (tiles of 2048 points). Live web
  views over the egui window would lag the code by a frame while scrolling,
  cover egui's menus and take the wheel and keys from vim; images stack and
  scroll like any card. Links are hit-tested against rects the page reports.
  The cost: no text selection inside rendered Markdown.
* The markdown plugin renders a file as one row, so the page's own spacing
  applies; ⌘M in the rendered view covers the whole document, the text view
  still comments on single lines. A row taller than the view scrolls by
  pixels under the motion keys.
* Threads send replies as data, not as text: a prefix like `↳ @bo:` broke a
  reply that started with a list or a fence.
