#!/usr/bin/env bash
# Build and install the kitchen on this mac:
#   * release builds of the app and kitchenctl (+ Linux kitchenctl for devboxes)
#   * ~/Applications/The Kitchen.app (plain bundle, ad-hoc signed; no Xcode needed)
#   * ~/.local/bin/kitchenctl -> the bundle's kitchenctl
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
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  codesign --force --deep -s - "$app" >/dev/null 2>&1 || echo "warn: ad-hoc codesign failed (app still runs locally)" >&2
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
echo "skill: link $root/skill/kitchen into your dotfiles skills/ so agents learn kitchenctl"
