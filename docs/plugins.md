# Writing kitchen plugins

A plugin is a folder of Lua. It can add commands, keybindings, viewer
diagnostics/highlights and `gd`/`gr` answers, and run any program — one-shot
or long-running (a language server) — locally or on the tab's devbox.
Why it works this way: [Design notes](#design-notes).

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
kitchen.keymap("viewer", "<leader>g", "hello.greet") -- viewer normal mode: <leader>g
```

Save and it's live: plugins reload when `plugin.toml` or any `*.lua` changes.
Run commands from **⌘⇧P** (command palette), their keys, or
`kitchenctl plugin run hello.greet`. `print()` and errors go to
`~/.kitchen/logs/plugins/hello.log` (`kitchenctl plugin logs hello`).

Examples in the repo's `plugins/`: `ruff` (Python lint), `python` (`gd`/`gr`
via the ty language server), `github` (PR review via ⌘⇧P),
`markdown` (⌘⇧V renders `.md` files in the viewer), `todo` (highlights), `prune-worktrees` (a
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
only one that refreshes) and `cli-token-sync` (every 5 minutes,
refreshes the mac's logins of OAuth CLIs when a token has 15 minutes or less
left, the way each CLI does it, and copies their access tokens, without
refresh tokens, to every devbox with an open workspace; a box's own refresh
token stays. Each CLI is a source in `lua/sources/`: `shipyard`
(`bin/shipyard`'s Okta login in `tokens.json`, into
`~/.config/shipyard/tokens.json` on the box; warns 7 days before the 90-day
login ends) and `buildkite` (`bin/bk`'s login in the mac's keychain, into
bk's `/dev/shm` store on the box, which a box restart wipes). `install.sh`
links `ruff`, `python`, `todo`, `github`, `markdown`, `prune-worktrees`,
`bazel-cache`, `dashboard`, `download`, `mcp-token-sync` and `cli-token-sync`
into `~/.kitchen/plugins/`.

## Configuration

`~/.kitchen/plugins.toml` overrides a plugin's `[config]` and can disable it:

```toml
[ruff]
select = "B,UP"
[todo]
enabled = false
```

Extra plugin folders: settings `plugin_dirs` (e.g. `~/Documents/dotfiles/kitchen-plugins`).

## API (level 16)

A manifest's `api = N` is the level it needs: a kitchen with a lower level
refuses the plugin (logged).
Besides `kitchen.lint` and `kitchen.lsp` (below), `require("kitchen.util")`
has the helpers the bundled plugins share (`q` shell quoting, `shell_path`,
`human`, `basename`, `sh`, `why`, `hosts`); see
`crates/kitchen-app/src/plugins/util.lua`.

### Events

```lua
kitchen.on("file_opened", function(ev) end)   -- the viewer loaded a file
kitchen.on("file_changed", function(ev) end)  -- …and it changed on disk since
kitchen.on("startup", function() end)         -- after init.lua ran
kitchen.on("stop", function() end)            -- before a reload or stop kills its processes
kitchen.on("open", function(ev) end)          -- a link typed into ⌘⇧P (see Views)
kitchen.on("comment", function(ev) end)       -- ⌘M … ⌘Enter in the viewer (see Comments)
kitchen.on("dashboard", function(ev) end)     -- a dashboard was shown / hidden (see Dashboard)
```

`ev.file = {host, worktree, path, abs}` identifies the file (pass it back to
`diagnostics.set`); `ev.filetype` (extension), `ev.version` (content hash),
`ev.text()` (contents; nil above 2 MB). Only files matching the manifest's
`filetypes` are sent. After a reload, the files viewers hold (the
newest 64) are sent again with `ev.replay = true`: their text may be older than
the file on disk.

### Commands and keys

```lua
kitchen.command(name, fn(ctx), { desc = "…", remote = false })  -- → "<plugin>.<name>"
kitchen.keymap("cmd+shift+y", target, { when = "viewer" })
kitchen.keymap("viewer", "<leader>xy", target)   -- <leader> then 1–3 keys, viewer normal mode
```

`target` is a command name or a function. `ctx = {workspace, tab, host,
worktree, viewer = {path, file, line, col, word, sel = {from, to}, dir, diff,
removed, version, menu}}` (`diff`: the viewer is in diff mode; `removed`: the
cursor is on a removed row, and `line` is the next current-file line): `file`
is `{host, worktree, path, abs}` like `ev.file`, `word` the identifier under
(or right after) the cursor and `col` the 1-based character of the raw line
where it starts. While the ⌘⇧S menu is up (a plugin chord still fires then), `menu` is
the `{label, section, url}` of the `kitchen.views` entry under its cursor, `{}`
on the tab's worktree row.
Chords need cmd, alt or ctrl. `when`: `any` (default) or `viewer`;
ctrl-only chords need `viewer` (terminals keep them). `remote = true` lets
`kitchenctl plugin run` from a devbox run the command.

`kitchen.keymap` sets a command's *default* keys. `[plugins]` in
`~/.kitchen/keybindings.toml` replaces them (`"ruff.explain" = ["<leader>x"]`)
in the places the plugin registered the command (chords: its `when`; leader
maps: the viewer). `<leader>` is `leader` in that file (`\` by default). A
default may not take a key a built-in binds where the chord applies, or an
`[edit]` key (the call errors); a keybindings.toml reload that makes one
collide drops it with the same error. The defaults are in
`crates/kitchen-app/assets/keybindings.toml`.

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
  on_stderr = function(chunk) end,   -- dropped without one
  on_exit = function(code) end,
}
p:write(bytes)   p:close()   -- close stdin      p:kill()
```

No timeout; at most 8 per plugin. Reloading or stopping the plugin runs its
`stop` handlers, closes every process's stdin, waits up to 0.5 s, then kills
what is left; a local process goes with its process group (the server a
`uvx` wrapper started), on a devbox the ssh goes and the remote process sees
EOF on stdin.

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
an error, a definition that is the asked word itself, or no answer within
20 s makes the viewer use its text search; `reply({})` means "none". The
viewer waits for the provider (no text search runs meanwhile), drops an
answer that arrives after the user opened another file, moved to another
line or pressed Esc, and doesn't ask on a removed diff row. The text search only
looks at files of the same language. `gd` jumps straight to a name bound in
the same function or file (a Python scope walk); other matches open a list,
imported modules first, then the nearest folders.

### Language servers

```lua
local server = require("kitchen.lsp").define {
  name = "ty", filetypes = { "py", "pyi" }, language_id = "python",
  root_markers = { "pyproject.toml", "setup.py" },   -- or tiers, see below
  cmd = function(root) return "exec uvx ty server" end,   -- a sh script
  prepare = function(ctx, done) done { settings = {}, init_options = {} } end,
  settings = {},         -- workspace/configuration answers, by section
  max_rss_mb = 6144,     -- restart a server using more memory (0: no cap)
  idle_minutes = 15,     -- stop a server nobody used for that long (0: never)
}
```

A plugin defines one server. It starts one per (host, project root) — the
nearest directory with a marker, up to the worktree — when the viewer opens a
matching file, keeps open files in sync, and provides `definition` /
`references`. Markers can
come in tiers, `{ { "ty.toml", { file = "pyproject.toml", contains =
"[tool.ty" } }, { "pyproject.toml" } }`: each tier walks up to the worktree
and the first tier with a match wins, so a file that names the project beats
a nearer one that only names a package. `prepare(ctx, done)` runs once per
root (`ctx = {host, root, worktree}`) before its server starts; the
`settings` and `init_options` it passes to `done` are that root's. Files outside
the worktree (an installed package you jumped into) use the server that
last answered on that host.

A new `gd` (or `gr`) cancels the one still running (`$/cancelRequest`), and
requests older than 20 s are cancelled. Every 5 s the client checks each
server's memory (its whole process tree) and restarts it above
`max_rss_mb`, stops servers idle for `idle_minutes`, and keeps at most 3
per host (starting a fourth stops the least recently used). Every exit is
logged with its code; a server that dies within 30 s three times is not
restarted until the plugin reloads. On unload the client cancels requests
and sends `exit` before the process is killed.

Servers learn about files that change on disk without the viewer (an
agent's edit, a checkout, a rebase) from the client: it polls `git status`
(and HEAD) under each server's root and sends `didChangeWatchedFiles`,
every 5 s or, on a big repo where `git status` is slow, every ten times as
long as it takes. A changed `uv.lock`, `ty.toml` or root `pyproject.toml`,
or more than 2000 changed files, restarts the server instead. Replayed
files are not opened on the server (it reads them from disk), and at most
32 documents stay open per server. `lsp.restart(server, host, root)` stops
and restarts one, once it isn't answering a `gd` / `gr`. See `plugins/python/init.lua`.

### Linters

```lua
require("kitchen.lint").define {
  sh = function(ev) return "ruff check --output-format=json --stdin-filename " .. ev.file.path .. " -" end,
  parse = function(res, ev) return { {line=…, col=…, message=…, severity=…} } end,
}
```

Runs on every `file_opened`/`file_changed` (the manifest's filetypes) on the
file's host, in its worktree, with the viewer's text on stdin; results for
superseded versions are dropped. See `plugins/ruff/init.lua`.

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
      { section = "main ← william/a", label = "#12 add retries", detail = "1/2", state = "merged",
        view = { rev = "refs/kitchen/pr/12", name = "kitchen-pr-12", fetch = { … }, base = "origin/main" } },
      { section = "main ← william/a", label = "#13 use them", detail = "2/2 · live", state = "open", approved = true,
        reviewers = { { state = "waiting", name = "pplx_aster", team = true, status = "requested", members = { "Paul Fryzel", "@max" } },
                      { state = "done", name = "@al", status = "approved" } },
        gates = { { state = "needed", label = "atlantis/apply-gate", note = "…still need a confirmed successful apply." } },
        description = "# use them\n\n…",
        view = { base = "origin/william/a" } },   -- no rev: the tab's own worktree
    },
  }
end)
```

⌘⇧S opens the tab's PR menu and sends `views` to every listener. A
listener answers with `kitchen.views`: the tab's whole menu, replacing the
last one (an open menu updates in place). Each entry's `view` takes
`kitchen.view`'s fields without `tab`. An entry's `state` (`"open"`,
`"draft"`, `"merged"` or `"closed"`) draws GitHub's icon for the PR before its
label; its `url` comes back to commands as `ctx.viewer.menu.url` while the
cursor is on it. `approved = true` draws a check after that icon. While
its row is on screen, the viewer's file list opens with its `reviewers`:
each a `state` (`"done"`, `"waiting"` on others, `"needed"` from the
author), a `name` (`team = true` draws it in that team's own color), a
`status` beside it, and `members` (a team's: their count below it, the names
on hover). Its `gates` follow under "Checks": what blocks the PR's merge,
each a `state`, a `label`, and a `note` shown on hover, or one done row
saying nothing does. Its `description` (Markdown) adds a "Description"
row above the changed files that shows it rendered as GitHub does; ⌘⇧K
from the first changed file goes there. A viewer on screen sends `views` itself too (when it
opens, when it shows a view the menu lacks, and every 2 minutes), so these
keep up without ⌘⇧S. Rows with the same `section` go together under that header. The kitchen adds a last row for the tab's
worktree against its diff base, unless an entry already shows exactly that.
Picking a row shows it. While its tab is selected, every row is checked out
and loaded off screen, one at a time, so a pick comes from the viewer cache
at once. With `have`, a row whose head commit differs from what its viewer
shows is checked out again and its viewer rebuilt off screen, replacing the
one on screen once loaded; without it, a pick refreshes the row's checkout
behind it.

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

## Design notes

The reasons behind the API, for whoever changes the host. History is in git.

* **The host owns rendering.** Plugins push data into namespaced,
  replaceable sets (neovim namespaces, Sublime `add_regions(key)`, VS Code
  `DiagnosticCollection`), and the host tracks every registration, so an
  unload or reload cleans up without the plugin's help. Decorations are never
  baked into `Doc`: per file the viewer keeps an overlay (spans, underlines,
  signs, virtual text) rebuilt only when the file's decoration generation or
  hash changes, and `CodeView` applies it while painting visible rows. The UI
  reads a per-file `Arc<FileDecor>` only when an atomic generation changed.
* **Plugin code never runs on the UI thread.** mlua with a vendored Lua 5.4,
  one Lua state per plugin on its own thread, so a hang stays in its plugin.
  WASM was deferred (edit → recompile, can't use the devbox's ruff), dylibs
  and other languages rejected. No lock is held while calling into Lua; host
  mutexes recover from poisoning and a panicking plugin thread is caught and
  marked failed, so a plugin can't wedge the UI.
* **Heavy work is external processes, run where the code lives** (the
  devbox), and the socket API (`decor.*`, `plugin.*`) gives external programs
  the same decorations as Lua. Pushes from the socket land in `ext.<ns>`;
  requests through the devbox relay can't reload plugins and run only
  `remote = true` commands.
* **Stale results are dropped, not raced.** Decorations carry the content
  hash (`blake3`, 24 hex) of the text they were made for; jobs carry the
  plugin's load generation, so results from before a reload are dropped. The
  viewer re-`stat`s its file every 3 s while visible and sends `file_changed`.
  Reload replays `file_opened` for the files the viewers hold. The watcher
  reloads once `plugin.toml` and `**/*.lua` (symlinks followed) have been
  stable for 300 ms, and after a load error waits for the next change.
* **File identity is `{host, worktree, path}`**: tabs have their own
  worktrees, so `(workspace, path)` is ambiguous. Columns are characters of
  the raw line (before tab expansion) so external tools can produce them.
* **Long-lived processes**: writes go through a writer thread (a child that
  stops reading can't block Lua), dropping the handle kills the child, and a
  local child leads its own process group, killed whole: a reload once
  killed only `uvx` and left an 11 GB ty running.
* **Providers can't wedge `gd`.** The host fails a plugin's pending requests
  when it unloads, and the viewer gives up after 20 s. The viewer doesn't race
  a text search against the provider: on agi the grep took 18–36 s, never
  won, and its late results jumped somewhere the user had left.
* **Language servers are kept on a leash**: on agi (35k Python files) ty grew
  to 27 GB answering one `gr`, so `kitchen.lsp` cancels superseded and 20 s
  old requests, restarts a server above `max_rss_mb` and keeps 3 per host.
  Servers learn of changes the viewer didn't make from a `git status` poll,
  since nothing else tells them. ty treats agi as one project from any
  directory, so nearest-marker roots started one whole-repo ty per
  sub-project; marker tiers follow ty's own rule.
* **`open`, `comment` and `views` are broadcasts**, not providers: providers
  are filetype-keyed and first-match only, and a link or a comment isn't tied
  to a file type. The core never stores comments (GitHub is the store for
  PRs), and GitHub knowledge lives only in `plugins/github`.
* **A PR is a view, not a tab**: checked out detached beside the repo and
  shown in the current tab's viewer, with no tmux session or agent of its
  own. The core owns the ⌘⇧S menu and the viewer cache; plugins only say what
  the rows are. The tab's agent lists its PRs in `$KITCHEN_PRS_FILE` because
  no branch history says reliably which branches a tab made (a reused spare
  brings its reflog along). Each switch is numbered and the UI drops an older
  `ShowViewer`, so a slow first checkout can't take the screen back.
* **Markdown is rendered as GitHub renders it**: GitHub's HTML (`gh api
  markdown`; comrak meanwhile and offline), GitHub's CSS, laid out by WebKit
  in web views parked off screen whose snapshots become egui textures. egui
  widgets couldn't match GitHub's spacing, list markers and bold, and live
  web views would lag the code by a frame while scrolling, cover egui's menus
  and take keys from vim. The cost: no text selection inside rendered
  Markdown.
