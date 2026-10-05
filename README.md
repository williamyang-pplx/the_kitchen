# the kitchen

A cmux-style macOS workspace manager for agent-driven PR work, written in Rust
(eframe/egui + alacritty_terminal).

- **Workspaces** in a vertical sidebar. Each is a repo on this mac or on a
  Perplexity devbox (any ssh alias). Setting up a devbox workspace pulls the
  latest dotfiles from `github.com/williamyang-pplx/dotfiles`, links skills
  from it, and installs `kitchenctl`. Drag workspaces to reorder them;
  right-click → Pin to top keeps one above the rest.
- **Edit tabs**: one PR each, in a fresh worktree + branch cut from the latest
  `origin/<default>`. Given a task, the agent starts with `/low-level-plan` and
  writes a plan you comment on inline (⌘⇧I).
- **Read-only tabs**: code review and understanding, on a shared read-only
  worktree kept at the latest `origin/<default>`. ⌘I asks for a PR link
  (`https://github.com/<org>/<repo>/pull/<n>`, any suffix): the `github`
  plugin opens that PR's head read-only in its own worktree, and the viewer
  shows its diff against the PR's base (as on GitHub). Re-opening the link
  picks up new pushes. Empty = latest main.
- File viewer. With changes (a PR tab, or an edit tab after the agent's
  edits, followed live) ⌘P opens straight on the first changed file's diff,
  next to a tree of just the changed files: j/k in the tree, or ⌘J/⌘K and
  `]c`/`[c` in the code, step through them; `a` in the list switches to the
  whole repo, ⌘B hides the list. In PR tabs the PR's review comments show
  under their lines, and ⌘M … ⌘Enter posts a comment (or a reply, on a
  commented line) to the PR on GitHub. Tabs without changes open on a folder view
  (`aster/`, set by `viewer_default_dirs`). Vim
  navigation (`-` up a folder, Enter opens), an fzf-style ⌘P file finder
  (space-separated terms, smart case), diff/plain mode, jump-to-definition and an
  outline mode with model-written summaries.
- **Plugins** (neovim-style Lua in `~/.kitchen/plugins/`): commands, keys,
  viewer diagnostics/highlights, `gd`/`gr` providers, and jobs or
  long-running processes (language servers) on the tab's devbox. Ships with
  `ruff` (Python lint), `python` (`gd` / `gr` from the ty language server),
  `github` (PR links in ⌘I, PR review comments) and `todo`. ⌘⇧P is the command palette.
  See [docs/plugins.md](docs/plugins.md).
- **`kitchenctl`**: the public CLI, so agents can do everything you do from
  the keyboard. See [docs/cli.md](docs/cli.md) or run `kitchenctl guide`.

## Build and run

```bash
scripts/install.sh              # release build, ~/Applications/The Kitchen.app,
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

Every app shortcut is ⌘+key, and acts on the focused pane: tab keys (⌘E ⌘I
⌘W ⌘1-9 ⌘H/L) only when the terminal is focused, panel keys (⌘M ⌘D ⌘O
⌘Enter) only in a focused side panel; the rest are global. Terminals get all other keys; the viewer uses vim
keys (`gd` definition, `gr` references, `Ctrl-o` back, `]d` next diagnostic, …).
`gd`/`gr` ask a plugin's language server when there is one (Python: ty) and
fall back to text search.

| keys | action |
|---|---|
| ⌘N | new workspace |
| ⌘J / ⌘K | next / previous workspace; in a focused viewer: next / previous changed file |
| ⌘1 … ⌘9 | go to tab N of the current workspace |
| ⌘H / ⌘L | previous / next tab; in a focused viewer: file list ↔ code (changed-files tree: j/k open the next / previous file, Enter/Esc back to code, `a` all files; folder list: j/k, gg/G, Enter/l open, h/- up) |
| ⌘E | new edit (read/write) tab — own worktree |
| ⌘I | new read-only tab: paste a PR link, or empty for latest main |
| ⌘R | rename the current tab |
| ⌘W | close tab |
| ⌘P | file viewer (again: file picker) |
| ⌘B | show / hide the viewer's file list |
| ⌘⇧I | plan reviewer |
| ⌘⇧R | rename the current workspace |
| ⌘⇧P | command palette (plugin commands, diagnostics, reload plugins) |
| ⌥H / ⌥L | focus the terminal pane (left) / side panel (right) |
| ⌘` | switch focus terminal ↔ side panel |
| ⌘M | comment on the cursor line / selection (viewer, PR tab: posted to the PR on GitHub; on a commented line: a reply) |
| ⌘Enter | submit comment (plan: send comments to agent) |
| ⌘D | toggle diff / plain file |
| ⌘O | toggle outline (collapsed) mode |
| ⌘. | close the side panel |
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
