# the kitchen

A cmux-style macOS workspace manager for agent-driven PR work, written in Rust
(eframe/egui + alacritty_terminal).

- **Workspaces** in a vertical sidebar. Each is a repo on this mac or on a
  Perplexity devbox (any ssh alias). Setting up a devbox workspace pulls the
  latest dotfiles from `github.com/williamyang-pplx/dotfiles`, links skills
  from it, and installs `kitchenctl`. Drag workspaces to reorder them;
  right-click → Pin to top keeps one above the rest.
- **Tabs**: one PR each, in a fresh worktree + branch cut from
  `origin/<default>` as last fetched (a background fetch every 5 minutes and
  when ⌘T opens, so ⌘T never waits on the network). Given a task, the agent starts with `/low-level-plan` and
  writes a plan you comment on inline (⌘⇧I). ⌘T also takes a PR link
  (`https://github.com/<org>/<repo>/pull/<n>`, any suffix): the `github`
  plugin opens that PR's branch in its own worktree, and the viewer shows its
  diff against the PR's base (as on GitHub). Re-opening the link (or `git
  pull`) picks up new pushes.
- File viewer. With changes (a PR tab, or a tab after the agent's
  edits, followed live) ⌘. opens straight on the first changed file's diff,
  next to a tree of just the changed files: j/k in the tree, or ⌘⇧J/⌘⇧K and
  `]c`/`[c` in the code, step through them; `a` in the list switches to the
  whole repo, ⌘B hides the list. In PR tabs the PR's review comments show
  under their lines, and ⌘M … ⌘Enter posts a comment (or a reply, on a
  commented line) to the PR on GitHub. Tabs without changes open on a folder view
  (`aster/`, set by `viewer_default_dirs`). Vim
  navigation (`-` up a folder, Enter opens), an fzf-style ⌘P file finder (in the viewer)
  (space-separated terms, smart case), diff/plain mode, jump-to-definition and an
  outline mode with model-written summaries.
- **Plugins** (neovim-style Lua in `~/.kitchen/plugins/`): commands, keys,
  viewer diagnostics/highlights, `gd`/`gr` providers, and jobs or
  long-running processes (language servers) on the tab's devbox. Ships with
  `ruff` (Python lint), `python` (`gd` / `gr` from the ty language server),
  `github` (PR links in ⌘T, PR review comments), `todo`, `prune-worktrees`
  (hourly, removes the worktrees of merged/closed PRs that no tab uses and
  that hold nothing unpushed) and `dashboard`. ⌘⇧P is the command palette.
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
```

Needs `git`, `tmux` and `claude` on each host. State is
in `~/.kitchen` (`KITCHEN_STATE_DIR` overrides it for an isolated test
kitchen). Tabs are tmux sessions on the kitchen's own server:
`tmux -L kitchen ls`.

### Agent skill

`skill/kitchen/SKILL.md` teaches agents `kitchenctl` (same text as
`kitchenctl guide`; regenerate with `scripts/sync-skill.py`). Skills are
symlinked from the dotfiles repo, so add it there — for example copy it to
`~/Documents/dotfiles/skills/helpers/kitchen/` — and it reaches devboxes on
the next workspace setup.

## Keybindings

A tab shows either its terminal or its side panel (the file viewer or the
plan) on the whole screen; ⌘. switches between the terminal and the viewer.
Every app shortcut is ⌘+key, and acts on what is shown: tab keys (⌘T ⌘W)
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
| ⌘⇧H / ⌘⇧L | in the viewer: file list ↔ code (changed-files tree: j/k open the next / previous file, Enter/Esc back to code, `a` all files; folder list: j/k, gg/G, Enter/l open, h/- up) |
| ⌘T | new tab — own worktree + branch; type a name, or paste a PR link for the PR's branch |
| ⌘-click | in a terminal: open the file path under the pointer in the viewer (at `path:line`; a partial path opens the worktree file whose path ends with it), or a URL in the browser; plugins add more with `kitchen.link` |
| ⌘R | rename the current tab |
| ⌘W | close tab |
| ⌘P | fuzzy file picker (in the viewer) |
| ⌘B | show / hide the viewer's file list |
| ⌘⇧I | plan reviewer |
| ⌘⇧R | rename the current workspace |
| ⌘⇧P | command palette (plugin commands, diagnostics, reload plugins) |
| ⌘M | comment on the cursor line / selection (viewer, PR tab: posted to the PR on GitHub; on a commented line: a reply) |
| ⌘Enter | submit comment (plan: send comments to agent) |
| ⌘D | workspace dashboard; in the viewer or plan: toggle diff / plain file |
| ⌘O | toggle outline (collapsed) mode |
| ⌘. | switch the whole tab between the terminal and the file viewer (from the plan: back to the terminal) |
| ⌘C / ⌘V | copy / paste |

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
