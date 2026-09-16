#!/usr/bin/env bash
# Build freenet-mobile for iOS and package it as an XCFramework with Swift bindings.
#
# Usage: crates/mobile/scripts/build-ios.sh [--debug] [--out DIR]
#
# Produces, under --out (default: target/ios):
#   FreenetMobile.xcframework/   static library + C header + module map, device and simulator
#   swift/FreenetMobile.swift    UniFFI-generated Swift API (add to the app target)
#
# Requirements: Xcode command line tools, rustup targets aarch64-apple-ios and
# aarch64-apple-ios-sim for the pinned toolchain (`rustup target add ... --toolchain 1.94.0`).
#
# Notes:
# - `-p freenet-mobile` is built in its own cargo invocation on purpose. Building
#   several workspace packages together lets Cargo unify features across them
#   (the "testing" feature incident documented in cross-compile.yml).
# - LZMA_API_STATIC makes lzma-sys (via xz2) link liblzma statically; CI sets
#   the same for macOS cross builds.
# - Only the pulley engine profile can run contracts on a real device (no JIT on
#   iOS); it is enabled through the `pulley` feature of the freenet dependency.
set -euo pipefail

PROFILE=release
CARGO_PROFILE_FLAG=--release
OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug) PROFILE=debug; CARGO_PROFILE_FLAG=""; shift ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT="${OUT:-$TARGET_DIR/ios}"
LIB=libfreenet_mobile.a
MODULE=FreenetMobile

export LZMA_API_STATIC=1
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"

cd "$ROOT"

echo "==> building static libraries ($PROFILE)"
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  cargo build -p freenet-mobile --lib $CARGO_PROFILE_FLAG --target "$target"
done

echo "==> generating Swift bindings"
GEN_PROFILE_FLAG=""
if [[ "$PROFILE" == debug ]]; then
  GEN_PROFILE_FLAG=--debug
fi
rm -rf "$OUT/gen" && mkdir -p "$OUT/swift"
"$ROOT/crates/mobile/scripts/generate-bindings.sh" $GEN_PROFILE_FLAG --out "$OUT/gen"
cp "$OUT/gen/$MODULE.swift" "$OUT/swift/"

echo "==> assembling XCFramework"
# Headers dir shared by both slices: the FFI header plus a module map that
# names the module the generated Swift imports.
HEADERS="$OUT/headers"
rm -rf "$HEADERS" && mkdir -p "$HEADERS"
cp "$OUT/gen/${MODULE}FFI.h" "$HEADERS/"
cat > "$HEADERS/module.modulemap" <<EOF
module ${MODULE}FFI {
    header "${MODULE}FFI.h"
    export *
}
EOF

rm -rf "$OUT/$MODULE.xcframework"
xcodebuild -create-xcframework \
  -library "$TARGET_DIR/aarch64-apple-ios/$PROFILE/$LIB" -headers "$HEADERS" \
  -library "$TARGET_DIR/aarch64-apple-ios-sim/$PROFILE/$LIB" -headers "$HEADERS" \
  -output "$OUT/$MODULE.xcframework"

echo "==> done"
echo "    $OUT/$MODULE.xcframework"
echo "    $OUT/swift/$MODULE.swift"
