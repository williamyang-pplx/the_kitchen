#!/usr/bin/env bash
# Cross-build kitchenctl for devboxes (static musl binaries) with cargo-zigbuild.
# The app uploads target/<triple>/release/kitchenctl to each devbox on setup.
#
#   scripts/build-cli-linux.sh            # x86_64 and aarch64
#   scripts/build-cli-linux.sh x86_64     # one arch
set -euo pipefail
cd "$(dirname "$0")/.."

command -v cargo-zigbuild >/dev/null || { echo "install with: cargo install cargo-zigbuild" >&2; exit 1; }
if ! command -v zig >/dev/null; then
  [ -x /opt/homebrew/opt/zig@0.15/bin/zig ] && export PATH="/opt/homebrew/opt/zig@0.15/bin:$PATH"
fi

# zig's linker warns about rustc's -O1 flag; it is harmless.
export RUSTFLAGS="${RUSTFLAGS:-} -A linker_messages"

arches=("${@:-x86_64 aarch64}")
for arch in ${arches[@]}; do
  triple="$arch-unknown-linux-musl"
  rustup target list --installed | grep -qx "$triple" || rustup target add "$triple"
  cargo zigbuild --release -p kitchenctl --target "$triple"
  echo "built target/$triple/release/kitchenctl"
done
