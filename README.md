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
  writes a plan you comment on inline (⌘⇧I). ⌘⇧A there asks the model about
  the plan's lines, with the agent's planning session (its Claude Code
  transcript) as context.
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
  commented line) to the PR on GitHub. ⌘⇧A asks the model (`ask_model`,
  sonnet) about the cursor line or selection; Enter sends the question and the
  answer shows under the line until Esc on it. Tabs without changes open on a folder view
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
  within 10 seconds, so Claude there stays logged in) and `dashboard`. ⌘⇧P is the command palette.
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

A tab shows either its terminal or its side panel (the file viewer or the
plan) on the whole screen; ⌘. switches between the terminal and the viewer,
and each tab remembers which one it was showing when you come back to it.
Every app shortcut is ⌘+key, and acts on what is shown: tab keys (⌘T ⌘I ⌘W)
only in the terminal, panel keys (⌘M ⌘O ⌘Enter) only in the viewer or plan;
the rest are global (⌘D: diff in the viewer or plan, the dashboard elsewhere). Terminals get all other keys; the viewer uses vim
keys (`gd` definition, `gr` references, `Ctrl-o` back, `]d` next diagnostic, …).
`gd`/`gr` ask a plugin's language server when there is one (Python: ty) and
fall back to text search; a slow one (over 0.4s) shows the text matches until it answers.

| keys | action |
|---|---|
| ⌘N | new workspace |
| ⌘J / ⌘K | next / previous workspace |
| ⌘1 … ⌘9 | go to tab N of the current workspace |
| ⌘H / ⌘L | previous / next tab |
| ⌘⇧J / ⌘⇧K | in the viewer: next / previous changed file |
| ⌘⇧D | in the viewer: the shown PR's description, rendered as on GitHub (title, author, state, base ← head); again, back to where you were |
| ⌘⇧S | PR menu: the PRs the tab's agent listed, one section per stack, plus those opened with ⌘⇧P (j/k, Enter shows, Esc closes); from a terminal it opens the viewer first |
| ⌘⇧H / ⌘⇧L | in the viewer: file list ↔ code (changed-files tree: j/k open the next / previous file, Enter/Esc back to code, `a` all files; folder list: j/k, gg/G, Enter/l open, h/- up) |
| ⌘T | new tab — own worktree + branch (a purple tab) |
| ⌘I | new tab in the workspace's main checkout — no worktree or branch, for quick tasks that need no PR (tabs share it; a yellow tab) |
| ⌘-click | in a terminal: open the file path under the pointer in the viewer (at `path:line`; a partial path opens the worktree file whose path ends with it), or a URL in the browser; plugins add more with `kitchen.link` |
| ⌘R | rename the current tab |
| ⌘W | close tab |
| ⌘⇧T | reopen the workspace's last closed tab (its agent resumes its conversation) |
| ⌘P | fuzzy file picker (in the viewer) |
| ⌘B | show / hide the viewer's file list |
| ⌘⇧I | plan reviewer |
| ⌘⇧R | rename the current workspace |
| ⌘⇧P | command palette (plugin commands, diagnostics, reload plugins); paste a PR link to review it |
| ⌘M | comment on the cursor line / selection (viewer, viewing a PR: posted to the PR on GitHub; on a commented line: a reply) |
| ⌘⇧A | in the viewer or plan: ask the model about the cursor line / selection (Enter asks; the answer shows under the line, Esc on it closes it) |
| ⌘Enter | submit comment (plan: send comments to agent) |
| ⌘D | workspace dashboard; in the viewer or plan: toggle diff / plain file |
| ⌘O | toggle outline (collapsed) mode |
| ⌘. | switch the whole tab between the terminal and the file viewer (from the plan: back to the terminal) |
| ⌘/ | list every shortcut |
| ⌘C / ⌘V | copy / paste; in a terminal, ⌘V with an image on the clipboard (or dropping files on it) pastes the files' paths, which Claude Code attaches. On a devbox the files are first copied to `~/.kitchen/attachments` there |

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
