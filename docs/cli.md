# kitchenctl reference

`kitchenctl` controls the kitchen from a shell. It is the public interface
for agents: anything William does with the keyboard, an agent can do here.
Every command has `--help` with examples; `kitchenctl guide` prints the agent
guide (also shipped as [`skill/kitchen/SKILL.md`](../skill/kitchen/SKILL.md)).

## Transport

- Newline-delimited JSON over a unix socket: one `Request`
  (`{"id", "method", "params", "caller"}`) per line, one `Response`
  (`{"id", "ok", "result" | "error"}`) per line. Types: `crates/kitchen-proto`.
- Socket: `$KITCHEN_SOCKET_PATH`, else `~/.kitchen/kitchen.sock`
  (`--socket PATH` overrides both). `KITCHEN_STATE_DIR` moves all of `~/.kitchen`.
- **On a devbox** the socket belongs to `kitchenctl __relay`, which the app runs
  over ssh (the devbox gateway blocks ssh port/socket forwarding). The relay
  wraps each request line in `{"rid", "line"}` frames on its stdout; the app
  answers on its stdin. The relay exits and removes its socket when ssh drops;
  the app restarts it with backoff (2s up to 60s).
- `caller` is filled from `KITCHEN_WORKSPACE_ID` / `KITCHEN_TAB_ID`, so
  commands default to the tab they run in.

## Handles

Workspaces: id, `workspace:N` or `N` (1-based sidebar order), or name.
Tabs: id, `tab:N` or `N` (1-based within the workspace), title, or tmux
session name. Without a handle: the caller's workspace/tab, then the one
selected in the window.

## Output and exit status

Default output is short text; `--json` (anywhere on the line) prints the raw
result. Exit codes: `0` ok, `1` the app refused (message on stderr; with
`--json` also `{"ok": false, "error"}` on stdout), `2` usage error, `3` the
app is not reachable.

## Commands

| command | API method | notes |
|---|---|---|
| `ping` | `system.ping` | |
| `version` | — | no app needed |
| `identify` | `system.identify` | caller workspace/tab, worktree, branch, plan file; focused ids |
| `guide` (`--skill`) | — | agent guide markdown; `--json` → `{topic, format, content}` |
| `rpc <method> [json]` | any | raw call |
| `workspace list` / `ls` | `workspace.list` | workspaces with tabs |
| `workspace current` | `workspace.current` | |
| `workspace new --repo P [--host H] [--name N]` | `workspace.create` | `--host` = `local` or an ssh alias; bootstraps the host in the background |
| `workspace select W` / `next` / `prev` | `workspace.select` / `.next` / `.prev` | ⌘J / ⌘K |
| `workspace rename NAME [-w W]` | `workspace.rename` | |
| `workspace close W` | `workspace.close` | ends tmux sessions; repo + worktrees stay |
| `workspace read [-w W \| --all]` | `workspace.mark_read` | clears the yellow unread dots; marks the tabs' notifications read |
| `workspace bootstrap [-w W]` | `workspace.bootstrap` | pull dotfiles, relink skills, install kitchenctl, start relay |
| `tab list [-w W]` | `tab.list` | |
| `tab new [--task T\|--task-file F] [--branch B] [--title T] [--command C] [--no-agent] [--no-focus] [-w W]` | `tab.create` | own worktree + new branch off fresh origin, or `--branch` (fetched; a tab on it is reused); `--task` starts `/low-level-plan` |
| `tab select T [-w W]` | `tab.select` | ⌘1…⌘9 |
| `tab rename TITLE [-t T]` | `tab.rename` | |
| `tab close [-t T]` | `tab.close` | ends the tmux session; worktree stays |
| `tab send TEXT [--enter] [-t T]` | `tab.send` | `-` reads stdin; multi-line text is pasted as one bracketed paste |
| `tab send-key KEY… [-t T]` | `tab.send_key` | tmux key names: `Enter`, `Escape`, `C-c`, `Up` |
| `tab read-screen [--scrollback N] [-t T]` | `tab.read_screen` | |
| `worktree list [--status] [-w W]` | `worktree.list` | role (main/edit/spare), branch, owning tab, dirty with `--status` |
| `worktree remove PATH [--force] [-w W]` | `worktree.remove` | refuses the main checkout, worktrees an open tab uses, and dirty ones without `--force` |
| `plugin timers` | `plugin.timers` | plugin timers (`kitchen.every`): interval, runs, last fired, next due, last error and stats |
| `viewer open [PATH] [--line N] [--diff] [-t T]` | `viewer.open` | |
| `plan path` / `open` / `ready` | `plan.path` / `.open` / `.ready` | `ready` reloads the reviewer and notifies William |
| `plan comments [--all]` | `plan.comments` | open (and sent) comments; `--all` includes resolved |
| `plan resolve ID` | `plan.resolve` | |
| `notify --title T [--body B] [-t T]` | `notify` | badge + macOS banner |
| `notifications [--unread] [--limit N]` | `notification.list` | |
| `debug screenshot PATH` | `debug.screenshot` | PNG of the window |

Hidden helpers: `__relay` (devbox tunnel, above), `__index` (symbol
extraction for devbox viewers; not wired up yet — it exits with an error),
and `__prompt-context` (UserPromptSubmit hook: asks `prompt.context` and
prints the note about the tabs a prompt mentions; silent on any error).

## Building for devboxes

`scripts/build-cli-linux.sh` cross-builds static musl binaries with
`cargo zigbuild`. Workspace setup uploads the matching one to
`~/.kitchen/bin/kitchenctl` on the devbox (skipped when the hash matches) and
links `~/.local/bin/kitchenctl` to it.
