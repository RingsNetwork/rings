#!/usr/bin/env bash
set -euo pipefail

CRATES="rings-derive rings-transport rings-snark rings-core rings-rpc rings-node"
INTERNAL_CRATES="rings-core rings-derive rings-node rings-rpc rings-snark rings-transport"
ROOT_MANIFEST="Cargo.toml"

usage() {
  cat <<'USAGE'
Usage: scripts/cargo-publish-crates.sh <check|dry-run|publish>

check    validate workspace package versions and internal path+version deps
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

line_for_key() {
  local manifest="$1"
  local section="$2"
  local key="$3"

  awk '
    function trim(value) {
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
      return value
    }
    $0 == "[" section "]" { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section {
      line = $0
      sub(/[[:space:]]+#.*$/, "", line)
      split(line, parts, "=")
      if (trim(parts[1]) == key) {
        print $0
        found = 1
        exit
      }
    }
    END { if (!found) exit 1 }
  ' section="$section" key="$key" "$manifest"
}

line_has_bool_true() {
  local line="$1"
  local key="$2"
  local normalized

  normalized="$(printf '%s\n' "$line" | sed 's/[[:space:]]#.*$//; s/[[:space:]]//g')"
  case "$normalized" in
    "$key=true"*|*"{${key}=true"*|*",${key}=true"*) return 0 ;;
    *) return 1 ;;
  esac
}

line_has_field() {
  local line="$1"
  local field="$2"
  local normalized

  normalized="$(printf '%s\n' "$line" | sed 's/[[:space:]]#.*$//; s/[[:space:]]//g')"
  case "$normalized" in
    "$field="*|*"{${field}="*|*",${field}="*) return 0 ;;
    *) return 1 ;;
  esac
}

line_string_field() {
  local line="$1"
  local field="$2"
  local value

  value="$(sed -nE "s/.*(^|[,{[:space:]])${field}[[:space:]]*=[[:space:]]*\"([^\"]+)\".*/\\2/p" <<<"$line")"
  [ -n "$value" ] || return 1
  printf '%s\n' "$value"
}

workspace_package_version() {
  local line

  line="$(line_for_key "$ROOT_MANIFEST" "workspace.package" "version")" || {
    echo "missing [workspace.package] version" >&2
    return 1
  }
  line_string_field "$line" "version"
}

package_uses_workspace_version() {
  local manifest="$1"
  local line

  line="$(line_for_key "$manifest" "package" "version.workspace")" || return 1
  line_has_bool_true "$line" "version.workspace"
}

package_version() {
  local manifest="$1"
  local line

  if package_uses_workspace_version "$manifest"; then
    workspace_package_version
    return
  fi

  line="$(line_for_key "$manifest" "package" "version")" || {
    echo "missing [package] version: $manifest" >&2
    return 1
  }
  line_string_field "$line" "version"
}

dependency_line() {
  local manifest="$1"
  local dep="$2"

  line_for_key "$manifest" "dependencies" "$dep" || true
}

workspace_dependency_line() {
  local dep="$1"

  line_for_key "$ROOT_MANIFEST" "workspace.dependencies" "$dep" || true
}

check_workspace_dependency() {
  local crate="$1"
  local manifest
  local expected_path
  local expected_version
  local actual_path
  local actual_version
  local line

  manifest="$(manifest_for "$crate")"
  expected_path="${manifest%/Cargo.toml}"
  expected_version="$(package_version "$manifest")"
  line="$(workspace_dependency_line "$crate")"

  if [ -z "$line" ]; then
    echo "missing workspace dependency for internal crate: $crate" >&2
    return 1
  fi

  if ! line_has_field "$line" "path" || ! line_has_field "$line" "version"; then
    echo "workspace internal dependency must include path and version: $ROOT_MANIFEST: $line" >&2
    return 1
  fi

  actual_path="$(line_string_field "$line" "path")"
  actual_version="$(line_string_field "$line" "version")"

  if [ "$actual_path" != "$expected_path" ]; then
    echo "workspace dependency path mismatch for $crate: expected $expected_path, got $actual_path" >&2
    return 1
  fi

  if [ "$actual_version" != "$expected_version" ]; then
    echo "workspace dependency version mismatch for $crate: expected $expected_version, got $actual_version" >&2
    return 1
  fi

  echo "ok workspace $crate $actual_version"
}

check_manifest() {
  local crate="$1"
  local manifest
  local version

  manifest="$(manifest_for "$crate")"
  version="$(package_version "$manifest")" || return 1

  if ! package_uses_workspace_version "$manifest"; then
    echo "publishable crate must inherit [workspace.package] version: $manifest" >&2
    return 1
  fi

  for dep in $INTERNAL_CRATES; do
    local line
    line="$(dependency_line "$manifest" "$dep")"
    if [ -z "$line" ]; then
      continue
    fi
    if ! line_has_bool_true "$line" "workspace"; then
      echo "internal dependency must inherit workspace dependency metadata: $manifest: $line" >&2
      return 1
    fi
    if line_has_field "$line" "path" || line_has_field "$line" "version"; then
      echo "internal dependency must not duplicate path or version: $manifest: $line" >&2
      return 1
    fi
  done

  echo "ok $crate $version"
}

run_check() {
  for crate in $INTERNAL_CRATES; do
    check_workspace_dependency "$crate"
  done

  for crate in $CRATES; do
    check_manifest "$crate"
  done
}

dependency_version() {
  local dep="$1"
  local line="$2"

  if line_has_bool_true "$line" "workspace"; then
    line="$(workspace_dependency_line "$dep")"
  fi

  line_string_field "$line" "version"
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
    version="$(dependency_version "$dep" "$line")"
    if ! crate_version_indexed "$dep" "$version"; then
      echo "skipping package $crate until crates.io indexes $dep $version"
      return 1
    fi
  done

  return 0
}

index_path_for_crate() {
  local crate="$1"
  local len="${#crate}"

  case "$len" in
    1) echo "1/$crate" ;;
    2) echo "2/$crate" ;;
    3) echo "3/${crate:0:1}/$crate" ;;
    *) echo "${crate:0:2}/${crate:2:2}/$crate" ;;
  esac
}

crate_version_indexed() {
  local crate="$1"
  local version="$2"
  local path

  path="$(index_path_for_crate "$crate")"
  curl -fsSL "https://index.crates.io/$path" 2>/dev/null | awk '
    index($0, "\"name\":\"" crate "\"") && index($0, "\"vers\":\"" version "\"") {
      found = 1
    }
    END { exit(found ? 0 : 1) }
  ' crate="$crate" version="$version"
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
    echo "waiting for crates.io index: $crate $version"
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
