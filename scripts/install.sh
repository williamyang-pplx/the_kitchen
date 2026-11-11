#!/usr/bin/env bash
# Build and install the kitchen on this mac:
#   * release builds of the app and kitchenctl (+ Linux kitchenctl for devboxes)
#   * ~/Applications/The Kitchen.app (plain bundle; no Xcode needed), signed with
#     "Kitchen Local Signing" if scripts/make-signing-cert.sh created it, else ad-hoc
#   * ~/.local/bin/kitchenctl -> the bundle's kitchenctl
#   * quits the running app and opens the new build (tmux sessions survive)
#
#   scripts/install.sh               # everything
#   scripts/install.sh --no-linux    # skip the devbox cross-build
#   scripts/install.sh --no-bundle   # just build + link kitchenctl
set -euo pipefail
cd "$(dirname "$0")/.."
root="$(pwd)"

linux=1
bundle=1
for arg in "$@"; do
  case "$arg" in
    --no-linux) linux=0 ;;
    --no-bundle) bundle=0 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

cargo build --release -p kitchen-app -p kitchenctl
[ "$linux" = 1 ] && scripts/build-cli-linux.sh

bin_dir="$root/target/release"
if [ "$bundle" = 1 ]; then
  app="$HOME/Applications/The Kitchen.app"
  macos="$app/Contents/MacOS"
  mkdir -p "$macos" "$app/Contents/Resources"
  cp "$bin_dir/kitchen-app" "$bin_dir/kitchenctl" "$macos/"
  cp assets/kitchen.icns "$app/Contents/Resources/kitchen.icns"
  # The app uploads these to devboxes (see kitchen_core::bootstrap::cli_binary_candidates).
  for triple in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    [ -f "target/$triple/release/kitchenctl" ] && cp "target/$triple/release/kitchenctl" "$macos/kitchenctl-$triple"
  done
  version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>The Kitchen</string>
  <key>CFBundleDisplayName</key><string>The Kitchen</string>
  <key>CFBundleIdentifier</key><string>ai.perplexity.william.kitchen</string>
  <key>CFBundleExecutable</key><string>kitchen-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>kitchen</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  # A stable identity keeps macOS privacy grants (Documents access) across
  # rebuilds; an ad-hoc signature changes with every build, so macOS asks again.
  if security find-identity -v -p codesigning 2>/dev/null | grep -q '"Kitchen Local Signing"'; then
    codesign --force --deep -s "Kitchen Local Signing" "$app"
  else
    codesign --force --deep -s - "$app" >/dev/null 2>&1 || echo "warn: ad-hoc codesign failed (app still runs locally)" >&2
    echo "note: ad-hoc signed — macOS re-asks for folder access after each rebuild; run scripts/make-signing-cert.sh once to fix" >&2
  fi
  cli="$macos/kitchenctl"
  echo "installed $app"
else
  cli="$bin_dir/kitchenctl"
fi

mkdir -p "$HOME/.local/bin"
dest="$HOME/.local/bin/kitchenctl"
if [ -e "$dest" ] && [ ! -L "$dest" ]; then
  echo "warn: $dest exists and is not a symlink; left alone" >&2
else
  ln -sfn "$cli" "$dest"
  echo "linked $dest -> $cli"
fi
# Bundled plugins: link the defaults into ~/.kitchen/plugins unless the user
# already has a plugin by that name (or removed the link on purpose: .skip-<name>).
plugins_dir="${KITCHEN_STATE_DIR:-$HOME/.kitchen}/plugins"
mkdir -p "$plugins_dir"
for p in ruff todo python github markdown prune-worktrees dashboard bazel-cache download mcp-token-sync; do
  if [ ! -e "$plugins_dir/$p" ] && [ ! -e "$plugins_dir/.skip-$p" ]; then
    ln -s "$root/plugins/$p" "$plugins_dir/$p"
    echo "plugin: linked $plugins_dir/$p"
  fi
done

if [ "$bundle" = 1 ]; then
  # Quit the installed app (dev builds from `cargo run` are left alone), wait
  # up to 10 s for it to exit, then open the new build.
  exe="$macos/kitchen-app"
  if pgrep -f "^$exe" >/dev/null; then
    osascript -e 'tell application id "ai.perplexity.william.kitchen" to quit' >/dev/null 2>&1 || true
    for _ in $(seq 50); do pgrep -f "^$exe" >/dev/null || break; sleep 0.2; done
    pkill -f "^$exe" || true
  fi
  # Launched from a kitchen tab, this shell has the tab's KITCHEN_* and tmux
  # variables; `open` would pass them on (KITCHEN_STATE_DIR from a test would
  # start the app on an empty state). Open it as the Dock would.
  (
    for v in $(env | sed -n 's/^\(KITCHEN_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$v"; done
    unset TMUX TMUX_PANE
    open "$app"
  )
  echo "restarted $app"
fi
