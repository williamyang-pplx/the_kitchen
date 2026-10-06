# the kitchen — design

A cmux-style macOS app, written entirely in Rust (eframe/egui + alacritty_terminal),
for agent-driven PR work across local repos and Perplexity devboxes.

## Crates

| crate | role |
|---|---|
| `kitchen-proto` | wire types for the socket API (`Request`/`Response`, `WorkspaceInfo`, env var names) |
| `kitchen-symbols` | tree-sitter symbol extraction (outline, jump-to-definition); also compiled into `kitchenctl` so devboxes index locally |
| `kitchen-core` | host abstraction (local / ssh with ControlMaster), git worktrees, tmux (`tmux -L kitchen`), SQLite, plan helpers, bootstrap |
| `kitchenctl` | the public CLI agents use; talks newline-JSON to `~/.kitchen/kitchen.sock` |
| `kitchen-app` | the GUI + API server |

## Model

- **Workspace** = repo on a host (`local` or ssh alias). Sidebar, ⌘J/⌘K;
  drag to reorder, pinned workspaces (`workspaces.pinned`) sort above the rest.
- **Tab** = tmux session `kt_<ws>_<tab>` on that host, attached through a pty.
  - every tab has its own worktree: `<repo>-<slug>` on a new branch `william/<slug>` off
    `origin/<default>` as last fetched, no fetch of its own; or an existing branch
    (or a rev, detached), fetched in the same round trip as its checkout. Opening a
    branch or rev a tab already has fast-forwards that tab (never a reset). With a
    task, the agent starts with `/low-level-plan` and writes `.kitchen/plan.md`.
  - *spares*: two detached worktrees `<repo>-kitchen-spare[-2]` kept at
    `origin/<default>`: a per-workspace warmer fetches it and fast-forwards them
    every 5 minutes and when the ⌘T dialog opens (also after every other fetch of
    it). A new tab takes one over instead of checking out the
    whole repo.
- `.kitchen/` inside each worktree is git-excluded and holds kitchen files.

## Threads

`Engine` (Arc, short locks) is shared by the UI thread, API connection threads and
background jobs. Slow work never holds a lock. The UI owns terminals and panel state;
the engine asks it to do UI things through `UiCommand`s.

## Remote

Devbox gateway blocks ssh port/socket forwarding, so devbox `kitchenctl` reaches the app
through `kitchenctl __relay`, run by the app over ssh: it listens on the devbox's
`~/.kitchen/kitchen.sock` and tunnels request lines over its stdio.

