#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FIXTURE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rings-swift-ffi.XXXXXX")"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

case "$(uname -s)" in
  Darwin)
    FIXTURE_LIBRARY="$FIXTURE_DIR/libfake_rings.dylib"
    cc -dynamiclib "$SCRIPT_DIR/../tests/fake_rings.c" -o "$FIXTURE_LIBRARY"
    ;;
  Linux)
    FIXTURE_LIBRARY="$FIXTURE_DIR/libfake_rings.so"
    cc -shared -fPIC "$SCRIPT_DIR/../tests/fake_rings.c" -o "$FIXTURE_LIBRARY"
    ;;
  *)
    echo "unsupported Swift smoke-test platform: $(uname -s)" >&2
    exit 2
    ;;
esac

swiftc "$SCRIPT_DIR/Rings.swift" "$SCRIPT_DIR/Smoke.swift" -o "$FIXTURE_DIR/rings-swift-smoke"
"$FIXTURE_DIR/rings-swift-smoke" "$FIXTURE_LIBRARY"

ACTUAL_LIBRARY="${1:-${RINGS_FFI_LIBRARY:-}}"
if [[ -n "$ACTUAL_LIBRARY" ]]; then
  if [[ ! -f "$ACTUAL_LIBRARY" ]]; then
    echo "real Rings FFI library not found: $ACTUAL_LIBRARY" >&2
    exit 1
  fi
  "$FIXTURE_DIR/rings-swift-smoke" --load-only "$ACTUAL_LIBRARY"
elif [[ "${RINGS_FFI_REQUIRE_LIBRARY:-0}" == "1" ]]; then
  echo "RINGS_FFI_REQUIRE_LIBRARY=1 but no real Rings FFI library was provided" >&2
  exit 1
fi
