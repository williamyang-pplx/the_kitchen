#!/usr/bin/env bash
# PR-switch bench: a hidden, isolated test kitchen on a fake devbox (this mac
# behind scripts/pr-switch-bench/ssh, FAKE_RTT seconds per round trip) whose
# repo is a big blob:none partial clone with PACKS promisor packs and a stack
# of PRS PRs that each change CHANGED files. It switches between the PRs with
# ⌘⇧S and prints how long each pick took to be on screen (the app's
# `kitchen::switch` log lines).
#
#   scripts/pr-switch-bench/run.sh             # build, make the repos, run
#   KEEP=1 scripts/pr-switch-bench/run.sh      # leave the temp dir for a look
#
# Env: FAKE_RTT (0.16, a devbox from the mac), FILES (60000), PRS (8),
#      CHANGED (400), PACKS (20000; keep it at most that, see below),
#      KITCHEN_BIN (target/release),
#      WAIT_COMPACT=1 (start once the kitchen has compacted the packs, as a
#      devbox's are after its first few minutes; needs a kitchen that does),
#      DWELL (seconds spent on each PR in round 1, reading it; 0),
#      PUSH_WAIT (seconds between the push and round 3; 30).
# Rounds: 1 every PR in order (first visits), 2 every PR again, reversed,
# 3 after a push to every other PR (GitHub reports new heads). A pick whose
# view isn't at the PR's head on GitHub is marked stale.
set -euo pipefail
# git keeps a descriptor open per pack it reads, up to this limit; with an
# agent shell's ~1M, a few gits over PACKS=100000 filled macOS's file table
# (kern.maxfiles, 276k) and took the mac down.
ulimit -n 10240

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=${KITCHEN_BIN:-$ROOT/target/release}
PRS=${PRS:-8}
T=$(mktemp -d /tmp/kbench-XXXX)

unset KITCHEN_WORKSPACE_ID KITCHEN_TAB_ID KITCHEN_PLAN_FILE KITCHEN_PRS_FILE KITCHEN_DIFF_BASE
export KITCHEN_HIDDEN=1 KITCHEN_STATE_DIR=$T/state KITCHEN_SOCKET_PATH=$T/state/kitchen.sock
export FAKE_HOME=$T/home FAKE_RTT=${FAKE_RTT:-0.16}
export PR_STACK_FILE=$T/stack.json
export PATH=$HERE:$PATH
mkdir -p "$KITCHEN_STATE_DIR/plugins" "$FAKE_HOME"
cp -R "$HERE/pr-stack" "$KITCHEN_STATE_DIR/plugins/"
# The fake devbox's dotfiles: a clone of a local repo, so devbox setup's pull
# works (a failed setup never starts the workspace's spare warmer, which
# compacts the packs).
git -c init.defaultBranch=main init -q "$T/dotfiles"
mkdir -p "$T/dotfiles/skills" && touch "$T/dotfiles/skills/.keep"
git -C "$T/dotfiles" add -A && git -C "$T/dotfiles" -c user.name=b -c user.email=b@b commit -qm dotfiles
git clone -q "$T/dotfiles" "$FAKE_HOME/.dotfiles"
K=$BIN/kitchenctl

echo "making the repos in $T/box…"
python3 "$HERE/make-repo.py" "$T/box" --files "${FILES:-60000}" --prs "$PRS" --changed "${CHANGED:-400}" --packs "${PACKS:-20000}" >"$PR_STACK_FILE"
REPO=$T/box/agi

APP=
WS=
cleanup() {
  [ -n "$APP" ] && kill "$APP" 2>/dev/null
  if [ -n "$WS" ]; then
    for s in $(tmux -L kitchen ls -F '#S' 2>/dev/null | grep "^kt_${WS}_" || true); do tmux -L kitchen kill-session -t "$s"; done
  fi
  if [ -z "${KEEP:-}" ]; then rm -rf "$T"; else echo "kept $T"; fi
}
trap cleanup EXIT

RUST_LOG=${RUST_LOG:-info,kitchen::exec=debug} "$BIN/kitchen-app" >"$T/app.log" 2>&1 &
APP=$!

# Watchdog: stop the run (the app and every git on the bench's repos) if the
# system's open files pass a quarter of kern.maxfiles, long before macOS runs
# out of them. The peak is printed at the end.
MAXFILES=$(sysctl -n kern.maxfiles)
(
  peak=0
  while kill -0 "$APP" 2>/dev/null; do
    n=$(sysctl -n kern.num_files)
    [ "$n" -gt "$peak" ] && peak=$n && echo "$peak" >"$T/files-peak"
    if [ "$n" -gt $((MAXFILES / 4)) ]; then
      echo "WATCHDOG: $n open files (max $MAXFILES): stopping the bench" >&2
      pkill -f "$T/" 2>/dev/null
      kill "$APP" 2>/dev/null
      exit 1
    fi
    sleep 0.5
  done
) &
for _ in $(seq 60); do [ -S "$KITCHEN_SOCKET_PATH" ] && break; sleep 0.5; done

WS=$("$K" --json workspace new --host fakebox --repo "$REPO" --name bench | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
"$K" tab new --title stack --no-agent >/dev/null
# The tab's viewer on its own worktree, once the worktree exists; then the
# viewer's first listing and the PR menu it asks for.
until "$K" viewer open pkg000/mod_00000.py >/dev/null 2>&1; do sleep 0.5; done
until grep -q "agi-stack || exit 3" "$T/app.log"; do sleep 0.5; done
sleep 3
if [ -n "${WAIT_COMPACT:-}" ]; then
  echo "waiting for the packs to be compacted…"
  for _ in $(seq 2400); do grep -q "packs compacted" "$T/app.log" && break; sleep 0.5; done
  grep 'packs compacted' "$T/app.log" | sed 's/.*INFO *[^ ]* /  /' | head -1
  grep 'path-format=absolute --git-path objects/pack' "$T/app.log" | sed -E 's/.*kitchen::exec\] +([0-9]+ms).*/  compaction took \1/' | head -1
fi

# Rows: the PRs in stack order, then the tab's worktree; the cursor opens on
# the row on screen.
at=$PRS
# A pick's line; "(moved)" lines are a shown PR rebuilt after its head moved.
switches() { grep 'kitchen::switch' "$T/app.log" | grep -vc '(moved)' || true; }
pick() {
  local to=$1 before key=j n
  before=$(switches)
  "$K" debug keys '<cmd-shift-s>' >/dev/null
  sleep 0.3
  # One key per frame: the menu moves at most one row a frame.
  n=$((to - at))
  if [ "$n" -lt 0 ]; then key=k n=$((-n)); fi
  while [ "$n" -gt 0 ]; do "$K" debug keys "$key" >/dev/null; sleep 0.2; n=$((n - 1)); done
  if [ -n "${SHOTS:-}" ]; then sleep 0.3; "$K" debug screenshot "$T/pick-$(switches).png" >/dev/null; fi
  "$K" debug keys '<cr>' >/dev/null
  at=$to
  for _ in $(seq 2400); do
    [ "$(switches)" -gt "$before" ] && break
    sleep 0.05
  done
  local line want head
  line=$(grep 'kitchen::switch' "$T/app.log" | grep -v '(moved)' | tail -1 | sed 's/.*kitchen::switch\] /  /')
  read -r want head < <(python3 -c "import json,sys; s=json.load(open(sys.argv[1])); i=int(sys.argv[2]); print(('#%d %s' % (s[i]['n'], s[i]['head'][:12])) if i < len(s) else 'the view -')" "$PR_STACK_FILE" "$to")
  case "$line" in
    "  $want at $head:"* | "  $want:"*) echo "$line" ;;
    "  $want at "*) echo "$line   << stale, GitHub's head is $head" ;;
    *) echo "$line   << wanted $want" ;;
  esac
}

echo "round 1: first visits"
for i in $(seq 0 $((PRS - 1))); do pick "$i"; sleep "${DWELL:-0}"; done
echo "round 2: back to each, reversed"
for i in $(seq $((PRS - 2)) -1 0) $((PRS - 1)); do pick "$i"; done

echo "round 3: a push to every other PR, then each again"
python3 - "$T/box/origin" "$PR_STACK_FILE" <<'EOF'
import json, subprocess, sys
origin, path = sys.argv[1], sys.argv[2]
stack = json.load(open(path))
git = lambda *a: subprocess.run(["git", "-C", origin, *a], check=True, capture_output=True, text=True).stdout.strip()
for p in stack[::2]:
    tree = git("rev-parse", p["head"] + "^{tree}")
    head = git("commit-tree", tree, "-p", p["head"], "-m", "push")
    git("update-ref", f"refs/pull/{p['n']}/head", head)
    git("update-ref", f"refs/heads/{p['branch']}", head)
    p["head"] = head
for p in stack:
    p["base_oid"] = git("rev-parse", p["base"])
json.dump(stack, open(path, "w"))
EOF
# The menu asks the plugin again (as the viewer does every 2 minutes).
"$K" debug keys '<cmd-shift-s>' >/dev/null; sleep 1; "$K" debug keys '<esc>' >/dev/null
sleep "${PUSH_WAIT:-30}"
for i in $(seq 0 $((PRS - 1))); do pick "$i"; done

sleep 5
echo "PRs on screen rebuilt after their head moved:"
grep 'kitchen::switch' "$T/app.log" | grep '(moved)' | sed 's/.*kitchen::switch\] /  /' || echo "  none"
echo
echo "peak open files (kern.num_files): $(cat "$T/files-peak" 2>/dev/null || echo ?) of $MAXFILES"
echo "git calls on the fake devbox (kitchen::exec), slowest:"
grep 'kitchen::exec' "$T/app.log" | sed -E 's/.*kitchen::exec\] +//' | sort -rn | head -8 | cut -c1-150
