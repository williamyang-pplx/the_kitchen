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
via the ty language server), `github` (PR review via ⌘⇧P),
`markdown` (⌘⇧V renders `.md` files in the viewer), `todo` (highlights), `hello` (commands, keys, jobs), `prune-worktrees` (a
`kitchen.every` timer; removes worktrees with no tab or tmux pane that were
untouched for `idle_hours`, 6 by default), `bazel-cache` (keeps the Bazel
caches on each workspace's host under `max_gb`, or above `min_free_percent` free, evicting
down to `target_gb`; the output bases of removed worktrees always go),
`dashboard` (worktrees, disk and RAM on the dashboard), `download` (a
`remote = true` command: a devbox tab runs `kitchenctl plugin run
download.pull` and its `~/outbox/` is rsynced to `~/Downloads/devbox/<host>/`
on the mac) and `mcp-token-sync` (every 5 minutes, copies the mac's MCP
OAuth tokens for the `servers` it lists, without refresh tokens, into
`~/.claude/.credentials.json` on every devbox with an open workspace, and
clears those servers from Claude's needs-auth cache there; a newly opened
devbox workspace is pushed to within 10 seconds; Claude on the mac stays the
only one that refreshes) and `shipyard-token-sync` (every 5 minutes,
refreshes the mac's `bin/shipyard` Okta login when it has 15 minutes or less
left, and copies its access token, without the refresh token, into
`~/.config/shipyard/tokens.json` on every devbox with an open workspace; a
box's own refresh token stays; warns 7 days before the 90-day login ends).
`install.sh` links `ruff`, `python`, `todo`, `github`, `markdown`,
`prune-worktrees`, `bazel-cache`, `dashboard`, `download`, `mcp-token-sync`
and `shipyard-token-sync` into `~/.kitchen/plugins/`.

## Configuration

`~/.kitchen/plugins.toml` overrides a plugin's `[config]` and can disable it:

```toml
[ruff]
select = "B,UP"
[todo]
enabled = false
```

Extra plugin folders: settings `plugin_dirs` (e.g. `~/Documents/dotfiles/kitchen-plugins`).

## API (level 12)

Level 12: comment items take `replies` and `repo` (see "Comments"); render rows are Markdown only (`text`, `spans` and the style keys are gone). Level 11: `kitchen.views` and the `views` event (the ⌘⇧S PR menu, see "Views"). Level 10: render rows take `markdown` and `lang`. Level 9 added `kitchen.render` (see "Renderings"). Level 8: `kitchen.view` replaces `kitchen.tab_open` (a PR is shown in a
tab's viewer, not opened as a tab), and `open` comes from ⌘⇧P with the
tab's command ctx. Level 7: `kitchen.tab_open` takes a `branch` (or a `rev`); there are no
read-only tabs any more, so tabs have no `kind` and `worktree.list` no
"readonly" role. Level 6 added `kitchen.every`, `kitchen.dashboard` and the `dashboard` event
(see "Timers" and "Dashboard"). Level 5 added `kitchen.link` (see "Links"). Level 4 added `kitchen.comments`, the `comment` event and
`kitchen.workspaces()` (see "Comments"). Level 3 added `kitchen.tab_open` and the `open` event. Level 2 added `kitchen.spawn`, `kitchen.provide`, `kitchen.json.null`,
`kitchen.lsp`, and `ctx.viewer.file` / `word`; `ctx.viewer.col` became a raw
column. Plugins written for level 1 keep working.

### Events

```lua
kitchen.on("file_opened", function(ev) end)   -- the viewer loaded a file
kitchen.on("file_changed", function(ev) end)  -- …and it changed on disk since
kitchen.on("startup", function() end)         -- after init.lua ran
kitchen.on("open", function(ev) end)          -- a link typed into ⌘⇧P (see Views)
kitchen.on("comment", function(ev) end)       -- ⌘M … ⌘Enter in the viewer (see Comments)
kitchen.on("dashboard", function(ev) end)     -- a dashboard was shown / hidden (see Dashboard)
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
worktree, viewer = {path, file, line, col, word, sel = {from, to}, dir, diff,
version}}` (`diff`: the viewer is in diff mode): `file` is `{host, worktree, path, abs}` like `ev.file`, `col` is
the cursor's 1-based character of the raw line, `word` the identifier under
it.
Chords need cmd, alt or ctrl; built-in shortcuts can't be taken (the call
errors). `when`: `any` (default), `terminal`, `viewer`, `plan`;
ctrl-only chords aren't allowed in terminals. `remote = true` lets
`kitchenctl plugin run` from a devbox run the command.

### Links

```lua
kitchen.link("AI-(\\d+)", function(ctx)    -- ⌘-click "AI-9586" in a terminal
  kitchen.run({ cmd = { "open", "https://linear.app/team/issue/AI-" .. ctx.link.groups[1] } })
end)
kitchen.link("PR-\\d+", "open_pr")        -- or a command name
```

⌘-clicking a match in a terminal tab runs `target` (a command name or a
function) with the command `ctx` plus `ctx.link = {text, groups}` (`groups`
are the capture groups, nil for one that didn't take part). The pattern is
Rust regex syntax, not a Lua pattern; a bad one makes `kitchen.link` error.
The kitchen matches the text itself, so Lua only runs on a click.

Rules are tried in order, first match under the pointer wins: OSC 8
hyperlinks, then plugin rules (in registration order), then the built-ins:
http(s) URLs (opened in the browser) and file paths (`path`, `path:line`,
`path:line:col`, `path#Lline`; opened in the viewer). A plugin rule shadows the
built-ins on the text it matches, so keep it no looser than its handler can
resolve.

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

### Views

```lua
kitchen.on("open", function(ev)   -- ev = the command ctx of the tab, plus `text`
  kitchen.view {
    tab = ev.tab,
    rev = "refs/kitchen/pr/12",  -- checked out detached in <repo>-kitchen-pr-12 (no tab, no agent)
    name = "kitchen-pr-12",
    fetch = { "+refs/pull/12/head:refs/kitchen/pr/12" },  -- fetched from origin first
    base = "origin/main",        -- the viewer diffs against merge-base(HEAD, this)
    have = {                     -- optional: skip the fetch when these revs already name these commits
      { rev = "refs/kitchen/pr/12", commit = "<headRefOid>" },
      { rev = "refs/remotes/origin/main", commit = "<baseRefOid>" },
    },
  }
end)
kitchen.view { tab = ctx.tab }   -- back to the tab's own worktree
```

A link (a URL or `owner/repo#N`) typed into ⌘⇧P goes to every plugin's
`open` handler; the one that recognises it calls `kitchen.view` (the rest
return). Fetch and checkout run in one round trip on the workspace's host (a
new worktree takes over a spare); the tab's viewer then shows that worktree,
with `ctx.viewer.worktree` set to it. With `have` (the commits GitHub
reports for the PR's head and base), the fetch is skipped when they are
already there, so going back to a PR that hasn't moved costs no round trip
to GitHub. Kitchens that don't know `have` ignore it and always fetch. See
`plugins/github/init.lua`.

```lua
kitchen.on("views", function(ev)   -- ⌘⇧S: the tab's command ctx, plus ev.prs (the lines
                                    -- of its $KITCHEN_PRS_FILE) and ev.branch (checked out, or nil)
  kitchen.views {
    tab = ev.tab,
    entries = {
      { section = "main ← william/a", label = "#12 add retries", detail = "1/2",
        view = { rev = "refs/kitchen/pr/12", name = "kitchen-pr-12", fetch = { … }, base = "origin/main" } },
      { section = "main ← william/a", label = "#13 use them", detail = "2/2 · live",
        view = { base = "origin/william/a" } },   -- no rev: the tab's own worktree
    },
  }
end)
```

⌘⇧S opens the tab's PR menu and sends `views` to every listener. A
listener answers with `kitchen.views`: the tab's whole menu, replacing the
last one (an open menu updates in place). Each entry's `view` takes
`kitchen.view`'s fields without `tab`. Rows with the same `section` go
together under that header. The kitchen adds a last row for the tab's
worktree against its diff base, unless an entry already shows exactly that.
Picking a row shows it. A view the tab showed before comes back at once
from the viewer cache, and its checkout is then refreshed (the viewer is
rebuilt if HEAD moved).

### Comments

```lua
kitchen.comments.set(ev.file, {
  { id = "123", line = 40, side = "new", title = "@al · L40", body = "use a `set`", dim = false,
    repo = "o/r", replies = { { title = "@bo · 2026-10-02", body = "agreed, see #12" } } },
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
* `body` and each reply's `body` are GitHub-flavored Markdown, rendered as
  GitHub renders them: with `repo`, by GitHub's own renderer (`gh api
  markdown`, in that repo's context, so `#12`, `@user` and SHAs link), and
  otherwise (or until GitHub answers, or offline) by comrak. GitHub's CSS
  styles it and WebKit lays it out off screen; the viewer draws the image.
  Each reply is drawn under its `title`. A click on a link opens it; any
  other click moves the cursor.
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

### Renderings

```lua
kitchen.render.set(ev.file, {
  { line = 1, end_line = 40, markdown = "# Title\n\n- one\n- **two**" },
}, { version = ev.version })
kitchen.render.clear(file_or_nil)
kitchen.render.show(ctx.tab, on)   -- on = nil toggles; tab = nil: the selected tab
```

A rendering replaces a file's text in the viewer while the viewer is in
rendered mode (`kitchen.render.show`; header `RENDERED`). Each row is
GitHub-flavored Markdown standing for source lines `line..=end_line`,
rendered as comments are (see "Comments"), in the workspace's GitHub repo.
Lines that no row covers are shown as text. A click on a link opens it, any
other click picks the row. With `version`, a rendering is shown only for
the text it was made from. In a diff, a row is marked added when one of its
lines is, and removed lines stay below it.

⌘M on a row comments on its source lines: `start_line` is the first line
of the first row, and `line` is the last line of the last row. `quote`
holds the source text. A comment shows under the row that holds its line.
A row taller than the viewer scrolls under j/k, ⌃d/⌃u, ⌃f/⌃b and gg/G
until its edge shows. `plugins/markdown` renders `.md` files this way and
binds ⌘⇧V: the whole document is one row, laid out in one pass as on GitHub
(YAML front matter shows as a `yaml` code block).

### Timers

```lua
local t = kitchen.every("pass", 3600, function()
  return { removed = 3, kept = 40 },          -- stats: name → number | string
         { [ws_id] = { removed = 1 } }        -- optional, per workspace
end, { desc = "remove done worktrees", first_delay = 60 })
t:run()   -- run it now (false while a run is going)
```

The kitchen runs `fn` every `interval` seconds (the first time after
`first_delay`, default one interval). It never starts a run while the last
one is still going, including across a reload, when the old load may still
be finishing it. Every run is recorded, kept across restarts (so a restart
doesn't bring the next run forward), and shown on the dashboard: runs, when
it last fired, how long it took, when it runs next, the error if it failed,
and the stats `fn` returned. `kitchenctl plugin timers` lists them too. A
long run may block in `kitchen.run_sync`; that's fine, but it holds up the
plugin's other callbacks meanwhile.

### Dashboard

⌘D (with the terminal focused), or the small grid at the right end of the
tab strip, shows the workspace's dashboard in place of the terminal: every
plugin timer, then the sections plugins set for that workspace.

```lua
kitchen.dashboard.set(ws_id, {          -- replaces this plugin's sections there
  { title = "Disk", note = "measured 15:20", rows = {
      { label = "/System/Volumes/Data", value = "412G / 926G", fraction = 0.44 },
      { label = "the_kitchen-notifications", value = "1.9G", mark = true },
  } },
})
kitchen.dashboard.clear(ws_id_or_nil)
kitchen.dashboard.showing()             -- ids of workspaces whose dashboard is on screen
kitchen.on("dashboard", function(ev) end)  -- ev = { workspace, open }
```

`fraction` (0–1) draws a bar; `mark` highlights the row. Measure only what's
on screen: refresh from a timer for `showing()` workspaces, and right away
on the `dashboard` event. See `plugins/dashboard`.

### Misc

`kitchen.notify(msg, "info"|"warn"|"error")` (toast), `kitchen.open(path, line,
{diff = bool})` (viewer; `diff` picks diff or plain mode, nil keeps the viewer's), `kitchen.log(...)`/`print(...)`, `kitchen.config`, `kitchen.name`,
`kitchen.json.encode/decode` (`kitchen.json.null` encodes as `null`),
`require()` from the plugin's `lua/` folder.

### Syntax grammars

Drop `.sublime-syntax` files in the plugin's `syntaxes/` folder; they're
added to the viewer's highlighter at startup (restart the kitchen after
changing them).

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
