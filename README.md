# the kitchen

A cmux-style macOS workspace manager for agent-driven PR work, written in Rust
(eframe/egui + alacritty_terminal).

- **Workspaces** in a vertical sidebar. Each is a repo on this mac or on a
  Perplexity devbox (any ssh alias). Setting up a devbox workspace pulls the
  latest dotfiles from `github.com/williamyang-pplx/dotfiles`, links skills
  from it, points `~/.bazelrc` at its `bazelrc` (the shared Bazel remote
  cache, on the mac too), and installs `kitchenctl`. Drag workspaces to reorder them;
  right-click → Pin to top keeps one above the rest.
- **Tabs**: one PR each, in a fresh worktree + branch cut from
  `origin/<default>` as last fetched (a background fetch every 5 minutes and
  when ⌘T opens, so ⌘T never waits on the network). Given a task, the agent starts with `/low-level-plan` and
  writes a plan you comment on inline (⌘⇧I). ⌘⇧A there asks the planning agent
  about the plan's lines.
- **PR review**: paste a PR link (`https://github.com/<org>/<repo>/pull/<n>`,
  any suffix, or `<org>/<repo>#<n>`) into ⌘⇧P and the tab's viewer shows that
  PR's diff against its base (as on GitHub), checked out beside the repo in
  `<repo>-kitchen-pr-<n>`. Each PR viewed gets a ⌘⇧P entry (`github.pr_<n>`);
  `github.own_changes` goes back to the tab's own changes.
- **A tab's PRs**: the tab's agent lists the PRs it opened in
  `.kitchen/tabs/<tab>/prs` (`$KITCHEN_PRS_FILE`, one number or link per
  line). ⌘⇧S shows them in a menu, one section per stack, then the PRs opened
  here with ⌘⇧P, then the tab's own worktree; j/k and Enter show one.
  Every viewer the tab showed is kept, so switching back is instant (cursor,
  folds and open file as you left them). While a PR is checked out and its
  files load, the viewer shows a frying pan tossing an egg.
- File viewer. With changes (a PR, or a tab after the agent's
  edits, followed live) ⌘. opens straight on the first changed file's diff,
  next to a tree of just the changed files: j/k in the tree, or ⌘⇧J/⌘⇧K and
  `]c`/`[c` in the code, step through them; `a` in the list switches to the
  whole repo, ⌘B hides the list. On a PR its review comments show
  under their lines, and ⌘M … ⌘Enter posts a comment (or a reply, on a
  commented line) to the PR on GitHub. Comments and ⌘⇧A answers render as
  Markdown, as GitHub renders them (GitHub's renderer and CSS, laid out by
  WebKit). ⌘⇧A asks the tab's agent about the cursor
  line or selection: the question goes to a fork of its Claude Code session
  (`claude -p --resume <id> --fork-session` on the tab's host), so the answer
  has the agent's context, model and tools, and the agent never sees it. It
  may not edit files. Tabs without a session ask `ask_model` (sonnet). Enter
  sends the question and the answer shows under the line, with a follow-up
  field under it (when the cursor is still there) that asks in the same
  thread; the fork is kept in `~/.claude/projects/-kitchen-asks/`. Esc in the
  field closes it and ⌘⇧A on the asked lines opens it again; Esc on them
  closes the thread. Tabs without changes open on a folder view
  (`aster/`, set by `viewer_default_dirs`). Vim
  navigation (`-` up a folder, Enter opens), an fzf-style ⌘P file finder (in the viewer)
  (space-separated terms, smart case), diff/plain mode, jump-to-definition and an
  outline mode with model-written summaries.
- **Plugins** (neovim-style Lua in `~/.kitchen/plugins/`): commands, keys,
  viewer diagnostics/highlights, `gd`/`gr` providers, and jobs or
  long-running processes (language servers) on the tab's devbox. Ships with
  `ruff` (Python lint), `python` (`gd` / `gr` from the ty language server),
  `github` (PR review via ⌘⇧P), `markdown` (⌘⇧V renders a Markdown file in
  the viewer; ⌘M still comments on it), `todo`, `prune-worktrees`
  (hourly, removes worktrees that no tab or tmux pane uses and that nobody
  touched for 2 days; branches stay), `bazel-cache` (every 30 minutes, keeps
  `~/bazel-cache` on the mac and each devbox under 30G: drops the output bases
  of removed worktrees, then the least recently used cache entries, never under
  a running build), `mcp-token-sync` (every 5 minutes, pushes the mac's MCP
  OAuth tokens, e.g. Slack's, to each open devbox, and to a newly opened one
  within 10 seconds, so Claude there stays logged in), `cli-token-sync`
  (every 5 minutes, refreshes the mac's `bin/shipyard` and `bin/bk` logins and
  pushes their access tokens to each open devbox, so Shipyard and Buildkite
  there need no login) and `dashboard`. ⌘⇧P is the command palette.
- **Dashboard** (⌘D, or the grid at the right end of the tab strip): each
  plugin timer's runs (last fired, next due, stats), and the workspace's
  worktrees, disk and RAM on its host, from the `dashboard` plugin.
  See [docs/plugins.md](docs/plugins.md).
- **`kitchenctl`**: the public CLI, so agents can do everything you do from
  the keyboard. See [docs/cli.md](docs/cli.md) or run `kitchenctl guide`.

## Build and run

```bash
scripts/install.sh              # release build, ~/Applications/The Kitchen.app (restarted),
                                # ~/.local/bin/kitchenctl, Linux kitchenctl for devboxes
scripts/install.sh --no-linux   # skip the devbox cross-build
cargo run -p kitchen-app        # development
scripts/build-cli-linux.sh      # just the devbox binaries (needs cargo-zigbuild + zig)
scripts/screenshots.sh [DIR]    # PNGs of every view, from an isolated test kitchen
```

Needs `git`, `tmux` and `claude` on each host. State is
in `~/.kitchen` (`KITCHEN_STATE_DIR` overrides it for an isolated test
kitchen). Tabs are tmux sessions on the kitchen's own server:
`tmux -L kitchen ls`. When a host restarts (a devbox resumed from pause),
a tab's session comes back in its worktree with its agent resuming the
conversation it was in.

### Agent skill

Every agent the kitchen starts gets the `kitchenctl` guide in its context:
a SessionStart hook runs `kitchenctl guide` (on this mac and on devboxes),
so nothing needs installing. The same text ships as `skill/kitchen/SKILL.md`
for agents started outside the kitchen; regenerate it with
`scripts/sync-skill.py`.

## Keybindings

Every key can be changed in `~/.kitchen/keybindings.toml`. The defaults are in
[`crates/kitchen-app/assets/keybindings.toml`](crates/kitchen-app/assets/keybindings.toml);
your file lists only what you change, and reloads within a second. A line
replaces all of an action's default keys (`quit = ["cmd+shift+q"]`), and `[]`
unbinds it.

Sections are places in the kitchen (global, terminal, panel, viewer, vim,
file list, plan, picker, menu, dialog, input, edit, plugins). A key bound in
an inner place wins over the same key outside it, so `cmd+d` is the diff in
the viewer and the dashboard elsewhere. A mistake shows a toast and keeps the
last good keymap. ⌘/ shows the live list, and every key hint in the window
reads it too.

A tab shows either its terminal or its side panel (the file viewer or the
plan) on the whole screen; ⌘. switches between the terminal and the viewer,
and each tab remembers which one it was showing when you come back to it.
By default, app shortcuts are ⌘+key and act on what is shown: tab keys (⌘T ⌘I ⌘W)
only in the terminal, panel keys (⌘M ⌘O ⌘Enter) only in the viewer or plan;
the rest are global (⌘D: diff in the viewer or plan, the dashboard elsewhere). Terminals get all other keys; the viewer uses vim
keys (`gd` definition, `gr` references, `Ctrl-o` back, `]d` next diagnostic, …).
`gd`/`gr` ask a plugin's language server when there is one (Python: ty) and
fall back to text search; a slow one (over 0.4s) shows the text matches until it answers.
In a terminal, ⌘-click opens the file path or URL under the pointer; ⌘V with
an image on the clipboard (or dropping files on it) pastes the files' paths,
which Claude Code attaches (on a devbox the files are first copied to
`~/.kitchen/attachments` there).

Plugin keys (`kitchen.keymap`) are defaults too: `[plugins]` in your file
replaces them, e.g. `"github.description" = ["cmd+shift+d"]`.

## Layout

| path | what |
|---|---|
| `crates/kitchen-app` | the GUI and the socket API server |
| `crates/kitchenctl` | the CLI (also the devbox relay) |
| `crates/kitchen-core` | hosts (local/ssh), git worktrees, tmux, SQLite, bootstrap |
| `crates/kitchen-proto` | wire types shared by app and CLI |
| `crates/kitchen-symbols` | symbol extraction for the viewer |
| `docs/design.md` | architecture |
| `docs/cli.md` | CLI and API reference |
