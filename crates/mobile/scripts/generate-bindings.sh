#!/usr/bin/env bash
# Build the host library, generate Swift bindings, and optionally check exports.
#
# Usage: crates/mobile/scripts/generate-bindings.sh [--debug] [--check] [--out DIR]

set -euo pipefail

PROFILE=release
CARGO_PROFILE_FLAG=--release
CHECK=false
OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug) PROFILE=debug; CARGO_PROFILE_FLAG=""; shift ;;
    --check) CHECK=true; shift ;;
    --out)
      [[ $# -ge 2 ]] || { echo "--out requires a directory" >&2; exit 2; }
      OUT="$2"
      shift 2
      ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
OUT="${OUT:-$TARGET_DIR/mobile-bindings}"
BUILD_OUTPUT="$(mktemp)"
trap 'rm -f "$BUILD_OUTPUT"' EXIT

cd "$ROOT"
echo "==> building host cdylib for uniffi-bindgen ($PROFILE)"
if ! cargo build -p freenet-mobile --lib $CARGO_PROFILE_FLAG \
  --message-format=json-render-diagnostics >"$BUILD_OUTPUT"; then
  cat "$BUILD_OUTPUT"
  exit 1
fi

# Read the path Cargo reported instead of assuming its target/profile layout.
LIBRARY="$(python3 - "$BUILD_OUTPUT" <<'PY'
import json
import sys

for line in open(sys.argv[1], encoding="utf-8"):
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    target = message.get("target", {})
    if message.get("reason") != "compiler-artifact":
        continue
    if target.get("name") != "freenet_mobile" or "cdylib" not in target.get("crate_types", []):
        continue
    for filename in message.get("filenames", []):
        if filename.endswith((".dylib", ".so", ".dll")):
            print(filename)
PY
)"
if [[ -z "$LIBRARY" || ! -f "$LIBRARY" ]]; then
  echo "could not find the freenet-mobile cdylib in Cargo build output" >&2
  exit 1
fi

mkdir -p "$OUT"
echo "==> generating Swift bindings"
cargo run -p freenet-mobile --bin uniffi-bindgen $CARGO_PROFILE_FLAG \
  --features bindgen -- generate --library "$LIBRARY" --language swift --out-dir "$OUT"

if [[ "$CHECK" == true ]]; then
  SWIFT="$OUT/FreenetMobile.swift"
  for name in FreenetNode MobileProfile ContractUpdateListener NodeStatus GetResult; do
    if ! grep -Fq "$name" "$SWIFT"; then
      echo "generated Swift is missing exported type: $name" >&2
      exit 1
    fi
  done
  echo "==> generated Swift contains the exported mobile types"
fi
