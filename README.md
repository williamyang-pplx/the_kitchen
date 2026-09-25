# the kitchen

A cmux-style macOS workspace manager for agent-driven PR work, written in Rust
(eframe/egui + alacritty_terminal).

- **Workspaces** in a vertical sidebar. Each is a repo on this mac or on a
  Perplexity devbox (any ssh alias). Setting up a devbox workspace pulls the
  latest dotfiles from `github.com/williamyang-pplx/dotfiles`, links skills
  from it, and installs `kitchenctl`.
- **Edit tabs**: one PR each, in a fresh worktree + branch cut from the latest
  `origin/<default>`. Given a task, the agent starts with `/low-level-plan` and
  writes a plan you comment on inline (⌘L).
- **Read-only tabs**: code review and understanding, on a shared read-only
  worktree kept at the latest `origin/<default>`.
- File viewer with vim navigation, diff/plain mode, jump-to-definition and an
  outline mode with model-written summaries; PR comments synced with GitHub;
  automatic agent reviews of watched people's PRs, where every agent comment
  waits for your approval before it goes public.
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

Needs `git`, `tmux`, `gh` (authenticated) and `claude` on each host. State is
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

Every app shortcut is ⌘+key. Terminals get all other keys; the viewer uses vim
keys (`gd`, `Ctrl-o`, …).

| keys | action |
|---|---|
| ⌘N | new workspace |
| ⌘J / ⌘K | next / previous workspace |
| ⌘1 … ⌘9 | go to tab N of the current workspace |
| ⌘E | new edit (read/write) tab — own worktree |
| ⌘R | new read-only tab — shared read-only worktree |
| ⌘W | close tab |
| ⌘P | file viewer (again: file picker) |
| ⌘L | plan reviewer |
| ⌘I | agent-review inbox |
| ⌘` | switch focus terminal ↔ side panel |
| ⌘M | comment on the cursor line / selection |
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
| `crates/kitchen-core` | hosts (local/ssh), git worktrees, tmux, gh, SQLite, bootstrap |
| `crates/kitchen-proto` | wire types shared by app and CLI |
| `crates/kitchen-symbols` | symbol extraction for the viewer |
| `docs/design.md` | architecture |
| `docs/cli.md` | CLI and API reference |
