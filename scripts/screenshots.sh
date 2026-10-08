#!/usr/bin/env bash
# Screenshot every view of an isolated test kitchen built from this checkout.
#
#   scripts/screenshots.sh [OUT_DIR]     # default /tmp/kitchen-shots
#
# The test kitchen gets its own state dir and socket, so it never talks to
# the real app (a kitchen tab exports KITCHEN_SOCKET_PATH, which would win
# over KITCHEN_STATE_DIR). Its tabs live on the shared `tmux -L kitchen`
# server; the script ends them when it exits.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/kitchen-shots}
TMP=$(mktemp -d /tmp/kitchen-shots-XXXX)

unset KITCHEN_WORKSPACE_ID KITCHEN_TAB_ID KITCHEN_PLAN_FILE KITCHEN_DIFF_BASE
export KITCHEN_STATE_DIR=$TMP/state
export KITCHEN_SOCKET_PATH=$TMP/state/kitchen.sock
mkdir -p "$KITCHEN_STATE_DIR/plugins" "$OUT"

cargo build -q --manifest-path "$ROOT/Cargo.toml" -p kitchen-app -p kitchenctl
K=$ROOT/target/debug/kitchenctl

# Throwaway clones, so worktrees land in $TMP.
git clone -q "$ROOT" "$TMP/the_kitchen"
git clone -q "$ROOT" "$TMP/dotfiles"
# Plugins that only read: dashboard fills the dashboard, markdown renders .md.
for p in dashboard markdown todo; do cp -R "$ROOT/plugins/$p" "$KITCHEN_STATE_DIR/plugins/"; done

"$ROOT/target/debug/kitchen-app" >"$TMP/app.log" 2>&1 &
APP=$!
WS_IDS=()
cleanup() {
    kill "$APP" 2>/dev/null || true
    for ws in "${WS_IDS[@]}"; do
        for s in $(tmux -L kitchen ls -F '#S' 2>/dev/null | grep "^kt_${ws}_" || true); do
            tmux -L kitchen kill-session -t "$s"
        done
    done
    rm -rf "$TMP"
}
trap cleanup EXIT

for _ in $(seq 1 30); do [ -S "$KITCHEN_SOCKET_PATH" ] && break; sleep 0.5; done
[ -S "$KITCHEN_SOCKET_PATH" ] || { echo "test kitchen did not start; see $TMP/app.log" >&2; exit 1; }

keys() { "$K" rpc debug.keys "{\"keys\":\"$1\"}" >/dev/null; sleep "${2:-0.6}"; }
shot() { sleep "${2:-1.5}"; "$K" debug screenshot "$OUT/$1.png" >/dev/null; echo "$OUT/$1.png"; }
ws_id() { "$K" --json workspace new --repo "$1" --name "$2" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])'; }

WS_IDS+=("$(ws_id "$TMP/the_kitchen" "the kitchen")")
WS_IDS+=("$(ws_id "$TMP/dotfiles" "Dev Ops")")
"$K" workspace select 1 >/dev/null
"$K" tab new --title "markdown rendering plugin" --no-agent >/dev/null
"$K" tab new --title "improve UI" --no-agent >/dev/null
"$K" tab new --title "fix scrollback" --no-agent --no-focus >/dev/null
"$K" notify --title "plan ready" -t 3 >/dev/null

# Changes in tab 1, so the viewer has a diff.
W1=$("$K" --json tab list | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["worktree"])')
python3 - "$W1" <<'EOF'
import sys, pathlib
root = pathlib.Path(sys.argv[1])
p = root / "crates/kitchen-app/src/theme.rs"
s = p.read_text()
s = s.replace("pub const BG:", "/// Example addition.\npub const EXAMPLE: f32 = 1.0;\npub const BG:", 1)
s = s.replace("/// System fonts used as fallbacks", "/// Fallback fonts", 1)
p.write_text(s)
r = root / "README.md"
r.write_text(r.read_text().replace("A cmux-style", "A cmux-style, keyboard-first", 1))
EOF

"$K" tab select 1 >/dev/null
"$K" tab send "clear; git log --oneline -12 && git diff --stat" --enter -t 1 >/dev/null
shot 01-terminal 2

"$K" viewer open crates/kitchen-app/src/theme.rs --diff --line 6 -t 1 >/dev/null
shot 02-viewer-diff 2.5
"$K" viewer open crates/kitchen-app/src/viewer/mod.rs --line 120 -t 1 >/dev/null
shot 03-viewer-file 2
keys "<cmd-p>"; keys "theme"
shot 04-finder
keys "<esc>"
keys "<cmd-shift-p>"
shot 05-palette
keys "<esc>"
keys "<cmd-.>"

keys "<cmd-d>" 3
shot 06-dashboard
keys "<cmd-d>"

PLAN=$("$K" plan path -t 2)
mkdir -p "$(dirname "$PLAN")"
cat >"$PLAN" <<'EOF'
# Plan: tasteful UI pass

## Goal

Make the kitchen feel calmer without changing the Tokyo Night palette.

## Steps

1. **Tokens.** Add spacing, radius and type-scale constants to `theme.rs`.
2. **Sidebar.** Quieter rows: name, host and repo, a status dot.
3. **Tab strip.** An underline marks the active tab.

| Area | Change | Risk |
|---|---|---|
| sidebar | quieter rows | low |
| viewer | hunk headers | medium |

- [ ] Screenshots before and after
EOF
"$K" tab select 2 >/dev/null
"$K" plan open -t 2 >/dev/null
shot 07-plan 2.5
keys "<cmd-.>"
keys "<cmd-n>"
shot 08-new-workspace
keys "<esc>"
keys "<cmd-r>"
shot 09-rename
keys "<esc>"
"$K" workspace select 2 >/dev/null
shot 10-empty-workspace
