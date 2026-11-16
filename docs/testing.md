# Testing the kitchen

## Unit tests

```bash
cargo test --workspace
```

Run them before you merge into `main`.

## UI changes: use a hidden test kitchen

To check a UI change, run a kitchen built from your checkout and look at
screenshots of it. Run it **hidden** (`KITCHEN_HIDDEN=1`), so William does
not see it and it does not take his keyboard or mouse. A hidden kitchen:

- draws every frame as usual, so `kitchenctl debug screenshot` works,
  including rendered Markdown;
- has a transparent window that ignores the mouse and sits off screen.
  macOS keeps about 40 px of it on a display, but those pixels are
  transparent and clicks go through them;
- has no Dock icon, never takes focus, and is not in ⌘\` or Mission Control;
- keeps its egui state (window size, position) in its own state dir, not in
  the live app's `~/Library/Application Support/the-kitchen/app.ron`;
- shows no macOS banners. Its in-app toasts still show in screenshots.

You cannot click a hidden kitchen. Drive it with `kitchenctl`: commands
(`tab new`, `viewer open`, …), keys (`kitchenctl debug keys '<cmd-p>main<cr>'`)
and screenshots (`kitchenctl debug screenshot /tmp/x.png`). Then read the
PNG to see the result.

### Keep it away from William's kitchen

- **Override the socket.** A kitchen tab exports `KITCHEN_SOCKET_PATH`
  (William's live `~/.kitchen/kitchen.sock`), and it wins over
  `KITCHEN_STATE_DIR`. Set both, for the app and for `kitchenctl`, and unset
  the tab's `KITCHEN_*` ids. If you don't, `kitchenctl` drives the live app.
- **Use a throwaway clone.** `tab new` makes worktrees next to the
  workspace's repo (`<repo>-<slug>`, `<repo>-kitchen-spare*`). Give the test
  kitchen a `git clone` in a temp dir, never a real checkout.
- **Copy only harmless plugins** into the test state dir (`markdown`,
  `dashboard`, `todo`). Never copy `bazel-cache` or `prune-worktrees`: they
  delete real files.
- **Clean up.** Tabs are sessions on the shared `tmux -L kitchen` server.
  Kill only the test workspace's `kt_<workspace id>_*` sessions, then the app.

### Recipe

```bash
cargo build -p kitchen-app -p kitchenctl
T=$(mktemp -d /tmp/ktest-XXXX)
unset KITCHEN_WORKSPACE_ID KITCHEN_TAB_ID KITCHEN_PLAN_FILE KITCHEN_PRS_FILE KITCHEN_DIFF_BASE
export KITCHEN_HIDDEN=1 KITCHEN_STATE_DIR=$T/state KITCHEN_SOCKET_PATH=$T/state/kitchen.sock
mkdir -p "$KITCHEN_STATE_DIR/plugins"
cp -R plugins/markdown "$KITCHEN_STATE_DIR/plugins/"
git clone -q . "$T/repo"
K=$PWD/target/debug/kitchenctl

target/debug/kitchen-app >"$T/app.log" 2>&1 &
APP=$!
for _ in $(seq 30); do [ -S "$KITCHEN_SOCKET_PATH" ] && break; sleep 0.5; done

WS=$("$K" --json workspace new --repo "$T/repo" --name test | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
"$K" tab new --title demo --no-agent
"$K" viewer open README.md
"$K" debug keys '<cmd-shift-v>'
sleep 3 && "$K" debug screenshot "$T/readme.png"   # then read the PNG

# Clean up.
kill "$APP"
for s in $(tmux -L kitchen ls -F '#S' 2>/dev/null | grep "^kt_${WS}_"); do tmux -L kitchen kill-session -t "$s"; done
```

Run every command in one shell, so the exports apply to all of them. Give
the UI a second or two after a command or key before you take a screenshot.
Rendered Markdown takes longer: WebKit lays it out off screen, and until it
is done the viewer shows the raw text in grey. If you see that, wait and take
the screenshot again.

`scripts/screenshots.sh [DIR]` does all of this for every view of the
kitchen. It also runs hidden.
