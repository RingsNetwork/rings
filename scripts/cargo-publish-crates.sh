#!/usr/bin/env bash
set -euo pipefail

CRATES="rings-derive rings-transport rings-snark rings-core rings-rpc rings-node"
INTERNAL_CRATES="rings-core rings-derive rings-node rings-rpc rings-snark rings-transport"

usage() {
  cat <<'USAGE'
Usage: scripts/cargo-publish-crates.sh <check|dry-run|publish>

check    validate crate-local package versions and internal path+version deps
dry-run  run cargo publish --dry-run when internal deps are indexed
publish  publish crates in dependency order, waiting for crates.io indexing
USAGE
}

manifest_for() {
  case "$1" in
    rings-core) echo "crates/core/Cargo.toml" ;;
    rings-derive) echo "crates/derive/Cargo.toml" ;;
    rings-node) echo "crates/node/Cargo.toml" ;;
    rings-rpc) echo "crates/rpc/Cargo.toml" ;;
    rings-snark) echo "crates/snark/Cargo.toml" ;;
    rings-transport) echo "crates/transport/Cargo.toml" ;;
    *) echo "unknown crate: $1" >&2; return 1 ;;
  esac
}

package_version() {
  awk '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
      value = $0
      sub(/^[^"]*"/, "", value)
      sub(/".*$/, "", value)
      print value
      found = 1
      exit
    }
    END { if (!found) exit 1 }
  ' "$1"
}

dependency_line() {
  local manifest="$1"
  local dep="$2"
  grep -E "^${dep}[[:space:]]*=" "$manifest" || true
}

check_manifest() {
  local crate="$1"
  local manifest
  local version

  manifest="$(manifest_for "$crate")"
  version="$(package_version "$manifest")" || {
    echo "missing concrete [package] version: $manifest" >&2
    return 1
  }

  if grep -q '^version\.workspace[[:space:]]*=' "$manifest"; then
    echo "workspace package version is not crate-local: $manifest" >&2
    return 1
  fi

  for dep in $INTERNAL_CRATES; do
    local line
    line="$(dependency_line "$manifest" "$dep")"
    if [ -z "$line" ]; then
      continue
    fi
    if [[ "$line" != *'path = '* || "$line" != *'version = '* ]]; then
      echo "internal dependency must include path and version: $manifest: $line" >&2
      return 1
    fi
  done

  echo "ok $crate $version"
}

run_check() {
  for crate in $CRATES; do
    check_manifest "$crate"
  done
}

run_dry_run() {
  run_check
  for crate in $CRATES; do
    if ! internal_dependencies_indexed "$crate"; then
      continue
    fi
    echo "dry-running publish $crate"
    cargo publish --dry-run -p "$crate" --allow-dirty
  done
}

internal_dependencies_indexed() {
  local crate="$1"
  local manifest
  manifest="$(manifest_for "$crate")"

  for dep in $INTERNAL_CRATES; do
    local line
    local version
    line="$(dependency_line "$manifest" "$dep")"
    if [ -z "$line" ]; then
      continue
    fi
    version="$(echo "$line" | sed -E 's/.*version = "([^"]+)".*/\1/')"
    if ! crate_version_indexed "$dep" "$version"; then
      echo "skipping package $crate until crates.io indexes $dep $version"
      return 1
    fi
  done

  return 0
}

crate_version_indexed() {
  local crate="$1"
  local version="$2"
  cargo search "$crate" --limit 1 --color never 2>/dev/null | grep -q "^${crate} = \"${version}\""
}

require_clean_worktree() {
  if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "refusing to publish from a dirty worktree" >&2
    return 1
  fi
}

wait_for_index() {
  local crate="$1"
  local version="$2"
  local attempts=40

  for _ in $(seq 1 "$attempts"); do
    if crate_version_indexed "$crate" "$version"; then
      echo "indexed $crate $version"
      return 0
    fi
    sleep 15
  done

  echo "timed out waiting for crates.io index: $crate $version" >&2
  return 1
}

run_publish() {
  run_check
  require_clean_worktree

  for crate in $CRATES; do
    local manifest
    local version
    manifest="$(manifest_for "$crate")"
    version="$(package_version "$manifest")"
    if crate_version_indexed "$crate" "$version"; then
      echo "already indexed $crate $version; skipping publish"
      continue
    fi
    echo "publishing $crate $version"
    cargo publish -p "$crate"
    wait_for_index "$crate" "$version"
  done
}

case "${1:-}" in
  check) run_check ;;
  dry-run) run_dry_run ;;
  publish) run_publish ;;
  -h|--help|help) usage ;;
  *) usage >&2; exit 2 ;;
esac
