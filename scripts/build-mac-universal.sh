#!/bin/bash
# Builds one projj binary that runs on both Apple-silicon and Intel Macs.
#
#   build-mac-universal.sh [OUT]
#
# OUT defaults to target/universal-apple-darwin/release/projj. Adds the Rust
# targets it needs (rustup target add) if they are missing.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-target/universal-apple-darwin/release/projj}"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

if [ "$(uname -s)" != Darwin ]; then
  echo "build-mac-universal.sh: needs a Mac (lipo and Apple's linker)" >&2
  exit 1
fi
installed="$(rustup target list --installed)"
for target in "${TARGETS[@]}"; do
  grep -qx "$target" <<<"$installed" || rustup target add "$target"
done

inputs=()
for target in "${TARGETS[@]}"; do
  cargo build --release --locked --target "$target"
  inputs+=("target/$target/release/projj")
done

mkdir -p "$(dirname "$OUT")"
lipo -create -output "$OUT" "${inputs[@]}"
lipo -info "$OUT"
# Rust's oldest supported macOS, as recorded in the binary.
echo "needs macOS $(otool -l -arch x86_64 "$OUT" | awk '/LC_VERSION_MIN_MACOSX/ { getline; getline; print $2; exit }') or later on Intel," \
  "$(otool -l -arch arm64 "$OUT" | awk '/LC_BUILD_VERSION/ { for (i = 0; i < 4; i++) { getline; if ($1 == "minos") { print $2; exit } } }') or later on Apple silicon"
