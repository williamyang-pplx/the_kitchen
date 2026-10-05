# Writing kitchen plugins

A plugin is a folder of Lua. It can add commands, keybindings, viewer
diagnostics/highlights and `gd`/`gr` answers, and run any program — one-shot
or long-running (a language server) — locally or on the tab's devbox.
Design and rationale: [plugins-design.md](plugins-design.md).

## Quick start

```
~/.kitchen/plugins/hello/plugin.toml
~/.kitchen/plugins/hello/init.lua
```

```toml
name = "hello"
description = "says hi"
api = 3
filetypes = []          # file events only for these extensions ([] = all)
[config]
greeting = "hi"
```

```lua
kitchen.command("greet", function(ctx)
  kitchen.notify(kitchen.config.greeting .. " from " .. (ctx.viewer.path or "?"))
end, { desc = "say hi" })
kitchen.keymap("cmd+shift+y", "hello.greet")        -- anywhere
kitchen.keymap("viewer", "<leader>g", "hello.greet") -- viewer normal mode: \g
```

Save and it's live: plugins reload when `plugin.toml` or any `*.lua` changes.
Run commands from **⌘⇧P** (command palette), their keys, or
`kitchenctl plugin run hello.greet`. `print()` and errors go to
`~/.kitchen/logs/plugins/hello.log` (`kitchenctl plugin logs hello`).

Examples in the repo's `plugins/`: `ruff` (Python lint), `python` (`gd`/`gr`
via the ty language server), `todo` (highlights), `hello` (commands, keys,
jobs). `install.sh` links `ruff`, `python` and `todo` into
`~/.kitchen/plugins/`.

## Configuration

`~/.kitchen/plugins.toml` overrides a plugin's `[config]` and can disable it:

```toml
[ruff]
select = "B,UP"
[todo]
enabled = false
```

Extra plugin folders: settings `plugin_dirs` (e.g. `~/Documents/dotfiles/kitchen-plugins`).

## API (level 4)

Level 4 added `kitchen.comments`, the `comment` event and
`kitchen.workspaces()` (see "Comments"). Level 3 added `kitchen.tab_open` and the `open` event (see "Tabs"). Level 2 added `kitchen.spawn`, `kitchen.provide`, `kitchen.json.null`,
`kitchen.lsp`, and `ctx.viewer.file` / `word`; `ctx.viewer.col` became a raw
column. Plugins written for level 1 keep working.

### Events

```lua
kitchen.on("file_opened", function(ev) end)   -- the viewer loaded a file
kitchen.on("file_changed", function(ev) end)  -- …and it changed on disk since
kitchen.on("startup", function() end)         -- after init.lua ran
kitchen.on("open", function(ev) end)          -- text typed into ⌘I (see Tabs)
kitchen.on("comment", function(ev) end)       -- ⌘M … ⌘Enter in the viewer (see Comments)
```

`ev.file = {host, worktree, path, abs}` identifies the file (pass it back to
`diagnostics.set`); `ev.filetype` (extension), `ev.version` (content hash),
`ev.lines`, `ev.text()` (contents; nil above 2 MB). Only files matching the
manifest's `filetypes` are sent. After a reload, every open file is sent again.

### Commands and keys

```lua
kitchen.command(name, fn(ctx), { desc = "…", remote = false })  -- → "<plugin>.<name>"
kitchen.keymap("cmd+shift+y", target, { when = "viewer", filetype = "py" })
kitchen.keymap("viewer", "<leader>xy", target)   -- \ then 1–3 keys, viewer normal mode
```

`target` is a command name or a function. `ctx = {workspace, tab, host,
worktree, viewer = {path, file, line, col, word, sel = {from, to}, dir,
version}}`: `file` is `{host, worktree, path, abs}` like `ev.file`, `col` is
the cursor's 1-based character of the raw line, `word` the identifier under
it.
Chords need cmd, alt or ctrl; built-in shortcuts can't be taken (the call
errors). `when`: `any` (default), `terminal`, `viewer`, `plan`;
ctrl-only chords aren't allowed in terminals. `remote = true` lets
`kitchenctl plugin run` from a devbox run the command.

### Diagnostics and highlights

```lua
kitchen.diagnostics.set(ev.file, {
  { line = 3, col = 5, end_col = 9, severity = "error", message = "…", code = "F401" },
}, { version = ev.version })
kitchen.diagnostics.clear(file_or_nil)
kitchen.highlights.set(ev.file, { { line = 1, col = 1, end_col = 5, group = "keyword" } }, { version = ev.version })
kitchen.highlights.clear(file_or_nil)
```

* Lines 1-based; columns 1-based **characters of the raw line** (a tab is one
  column), `end_col` exclusive; `end_line` optional.
* `severity`: `error`, `warning`, `info`, `hint` (or LSP numbers 1–4).
* `group`: a TextMate scope colored by the theme (`keyword`, `string`,
  `comment`, `entity.name.function`, `constant.numeric`, …), `error`/
  `warning`/`info`/`todo`, or `#rrggbb`.
* Each call replaces the plugin's previous set for that file (optionally per
  sub-namespace: `{ ns = "extra" }`). With `version`, the set hides itself
  once the file's text changes.
* The viewer shows a gutter sign, an underline, the first message after the
  line, and the full message on the status line. `]d` / `[d` jump between
  diagnostics; ⌘⇧P → "diagnostics in this file" lists them.

### Processes

```lua
kitchen.run({ cmd = {"ruff", "check", "-"}, host = ev.file.host, cwd = ev.file.worktree,
              stdin = ev.text(), timeout = 30 }, function(res) end)
local res = kitchen.run_sync({ sh = "git log -1 --format=%s", cwd = ctx.worktree })
-- res = {code, stdout, stderr, timed_out}
```

`host` is `"local"` or the devbox alias (`ev.file.host`, `ctx.host`); remote
jobs run over the workspace's ssh connection. `sh = "…"` runs a shell
script. A timeout kills the job. Results that arrive after a reload are
dropped. Non-interactive ssh shells have a minimal PATH — prefer
`sh = 'export PATH="$HOME/.local/bin:$PATH"; …'`.

### Long-running processes

```lua
local p = kitchen.spawn {
  sh = "exec my-server --stdio", host = ctx.host, cwd = root,   -- or cmd = {…}
  on_stdout = function(chunk) end,   -- raw bytes, as they arrive
  on_stderr = function(chunk) end,   -- default: lines go to the plugin log
  on_exit = function(code) end,
}
p:write(bytes)   p:close()   -- close stdin      p:kill()   p:alive()   p.id
```

No timeout; at most 8 per plugin. Reloading or stopping the plugin kills
them (on a devbox the ssh goes, and the remote process sees EOF on stdin).

### gd / gr providers

```lua
kitchen.provide("definition", function(ctx, reply)   -- or "references"
  reply({ { path = "pkg/a.py", line = 12, col = 5 } })  -- nil = "can't tell"
end, { filetypes = { "py" } })
```

The viewer asks the provider for the file's type on `gd` / `gr`. `reply`
may be called later (after a server answers). `path` is worktree-relative,
or absolute for files outside it; `col` is a raw 1-based character; an
optional `text` is the preview line (filled in when missing). `reply(nil)`,
an error, or no answer within 20 s makes the viewer use its text search;
`reply({})` means "none".

### Language servers

```lua
require("kitchen.lsp").define {
  name = "ty", filetypes = { "py", "pyi" }, language_id = "python",
  root_markers = { "pyproject.toml", "setup.py" },
  cmd = function(root) return "exec uvx ty server" end,   -- sh, or an argv table
  settings = {},         -- workspace/configuration answers, by section
  diagnostics = false,   -- show publishDiagnostics
}
```

Starts one server per (host, project root) — the nearest directory with a
marker, up to the worktree — when the viewer opens a matching file, keeps
open files in sync, and provides `definition` / `references`. Files outside
the worktree (an installed package you jumped into) use the server that
last answered on that host. Servers that die within 30 s three times are
not restarted until the plugin reloads. See `plugins/python/init.lua`.

### Linters

```lua
require("kitchen.lint").define {
  filetypes = { "py" },                                   -- default: manifest's
  sh = function(ev) return "ruff check --output-format=json --stdin-filename " .. ev.file.path .. " -" end,
  parse = function(res, ev) return { {line=…, col=…, message=…, severity=…} } end,
}
```

Runs on every `file_opened`/`file_changed` on the file's host, in its
worktree, with the viewer's text on stdin; results for superseded versions
are dropped. See `plugins/ruff/init.lua`.

### Tabs

```lua
kitchen.on("open", function(ev)   -- ev = { text, workspaces = { {id, name, host, repo_path,
  -- github_slug, default_branch, selected, tabs}, … } }
  kitchen.tab_open {
    workspace = ws.id,
    rev = "refs/kitchen/pr/12",                           -- what to check out (any rev)
    fetch = { "+refs/pull/12/head:refs/kitchen/pr/12" },  -- fetched from origin first
    diff_base = "origin/main",  -- the viewer diffs against merge-base(HEAD, this)
    name = "pr-12",             -- worktree <repo>-kitchen-ro-pr-12; reopening reuses the tab
    title = "#12 Fix it",
  }
end)
```

⌘I (and `kitchenctl tab open TEXT`) opens a read-only tab on the latest
default branch when the text is empty; otherwise every plugin's `open`
handler gets the text, and the one that recognises it calls
`kitchen.tab_open` (the rest return). Fetch and checkout run in one round
trip on the workspace's host; the worktree is read-only. Opening the same
`name` again re-fetches, moves the worktree only if the rev moved, and
reloads the viewer. Every tab's shell gets `KITCHEN_DIFF_BASE`. See
`plugins/github/init.lua` (PR links).

### Comments

```lua
kitchen.comments.set(ev.file, {
  { id = "123", line = 40, side = "new", title = "@al · L40", body = "use a set", dim = false },
  { body = "about the whole file" },               -- no line: drawn above the first line
})
kitchen.comments.clear(file_or_nil)

kitchen.on("comment", function(ev)
  -- ev = { file, path, line, side, start_line, start_side, text, quote, reply_to, tab }
end)
```

* Comments are notes drawn under their line (the last line of a range).
  `side = "old"` numbers lines in the diff base (a removed line; only shown
  on a diff), `"new"` (default) in the current file. Like other
  decorations, each call replaces the plugin's set for that file.
* ⌘M in the viewer opens a comment field under the cursor line (or the `V`
  selection); ⌘Enter hands it to every `comment` handler and closes it, Esc
  cancels. A removed line gives `side = "old"`. `start_line`/`start_side`
  are set for ranges, `quote` is the lines' text, `tab` is the command
  context. On a row that already has a comment with an `id`, ⌘M replies:
  `reply_to` is that id. The plugin posts it and sets the comments again
  (show a faded "posting…" item meanwhile, and keep a failed one visible so
  the text isn't lost). With no handler, ⌘M says so.
* `kitchen.workspaces()` returns the workspaces (and their tabs) as in the
  `open` event, e.g. to find the tab a file's worktree belongs to.
* `plugins/github` uses this for PR review comments (stored on GitHub).

### Misc

`kitchen.notify(msg, "info"|"warn"|"error")` (toast), `kitchen.open(path, line)`
(viewer), `kitchen.log(...)`/`print(...)`, `kitchen.config`, `kitchen.name`,
`kitchen.json.encode/decode` (`kitchen.json.null` encodes as `null`),
`require()` from the plugin's `lua/` folder.

### Syntax grammars and themes

Drop `.sublime-syntax` files in the plugin's `syntaxes/` folder; they're
added to the viewer's highlighter at startup (restart the kitchen after
changing them). A plugin grammar wins over a bundled one for the same
extension, and `file_extensions` may hold a whole file name (`go.mod`),
which is tried before the extension. Grammars run on syntect, which ignores
`extends`, `branch_point`/`fail` and `pop: N`; a grammar that fails to
compile is logged and skipped. A plugin can be grammars only: no `init.lua`.

Put Sublime syntax tests in `tests/syntax_test_*` (first line
`# SYNTAX TEST "Packages/X/Name.sublime-syntax"`, where `Name` is the
grammar's file name; then `# ^^^ scope` / `# <- scope` assertions);
`cargo test -p kitchen-app plugin_syntax_tests` runs every bundled
plugin's tests (`KITCHEN_PLUGIN_TEST=<name>` runs one plugin's).

A `themes/*.tmTheme` replaces the default color theme (the first one found
in an enabled plugin); only foreground colors are used.

Bundled language plugins: `tokyonight` (theme), `python`, `yaml`,
`terraform` (`.tf`, `.tfvars`, `.hcl`), `go` (plus `go.mod`), `rust`,
`typescript` (`.ts`, `.tsx`, `.mts`, `.cts`).

## Limits

Each callback may run 500 M Lua instructions (waiting on jobs doesn't count),
a plugin may use 256 MB and run 8 processes, a set holds 10 000 items. Errors are logged and
toasted once; a crashing plugin is marked failed (`kitchenctl plugin list`)
and restarts when you edit it.

## From other programs

Without Lua, any process can push diagnostics (they land in `ext.<ns>`):

```
kitchenctl decor set app/main.py --ns mytool --items '[{"line":3,"message":"boom"}]'
```
