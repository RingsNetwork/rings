#!/usr/bin/env bash
set -euo pipefail

# Publish order is a topological sort of the internal dependency graph: every crate
# appears after all internal crates it depends on. `rings-codec`, `rings-runtime`,
# `rings-aez`, `rings-network-policy`, `rings-dns-relay`, and `rings-gateway` must ship
# before their dependents; `rings-webview` depends on `rings-network-policy` and has no
# workspace dependents.
DEFAULT_CRATES="rings-derive rings-codec rings-measure rings-runtime rings-aez rings-network-policy rings-dns-relay rings-gateway rings-webview rings-transport rings-core rings-rpc rings-node"
CRATES="${RINGS_PUBLISH_CRATES:-$DEFAULT_CRATES}"

# Internal crates that appear as `{ workspace = true }` dependencies of a publishable
# crate; used to validate workspace dependency metadata and to gate publishing on
# crates.io indexing. `rings-webview` is omitted: it is published but never depended on,
# so it has no `[workspace.dependencies]` entry to validate.
DEFAULT_INTERNAL_CRATES="rings-aez rings-codec rings-core rings-derive rings-dns-relay rings-gateway rings-measure rings-network-policy rings-node rings-rpc rings-runtime rings-transport"
INTERNAL_CRATES="${RINGS_INTERNAL_CRATES:-$DEFAULT_INTERNAL_CRATES}"
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
    rings-aez) echo "crates/aez/Cargo.toml" ;;
    rings-codec) echo "crates/codec/Cargo.toml" ;;
    rings-core) echo "crates/core/Cargo.toml" ;;
    rings-derive) echo "crates/derive/Cargo.toml" ;;
    rings-dns-relay) echo "crates/dns-relay/Cargo.toml" ;;
    rings-gateway) echo "crates/gateway/Cargo.toml" ;;
    rings-measure) echo "crates/measure/Cargo.toml" ;;
    rings-network-policy) echo "crates/network-policy/Cargo.toml" ;;
    rings-node) echo "crates/node/Cargo.toml" ;;
    rings-rpc) echo "crates/rpc/Cargo.toml" ;;
    rings-runtime) echo "crates/runtime/Cargo.toml" ;;
    rings-transport) echo "crates/transport/Cargo.toml" ;;
    rings-webview) echo "crates/webview/Cargo.toml" ;;
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

dependency_lines() {
  local manifest="$1"
  local dep="$2"

  awk '
    function trim(value) {
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
      return value
    }
    /^\[/ {
      section = $0
      gsub(/^\[|\]$/, "", section)
      in_section = (section == "dependencies" || section == "build-dependencies" || section ~ /^target\..*\.(dependencies|build-dependencies)$/)
      next
    }
    in_section {
      line = $0
      sub(/[[:space:]]+#.*$/, "", line)
      split(line, parts, "=")
      if (trim(parts[1]) == key) {
        print $0
      }
    }
  ' key="$dep" "$manifest"
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
    while IFS= read -r line; do
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
    done < <(dependency_lines "$manifest" "$dep")
  done

  echo "ok $crate $version"
}

check_publish_graph() {
  cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys

crates = sys.argv[1].split()
internal_crates = set(sys.argv[2].split())
metadata = json.load(sys.stdin)

if len(crates) != len(set(crates)):
    print("duplicate entries in CRATES publish list", file=sys.stderr)
    sys.exit(1)

packages = {pkg["name"]: pkg for pkg in metadata["packages"]}
publishable = {
    name
    for name, pkg in packages.items()
    if pkg["publish"] is None or len(pkg["publish"]) > 0
}

missing_publishable = sorted(publishable - set(crates))
if missing_publishable:
    print(f"publishable workspace crates missing from CRATES: {missing_publishable}", file=sys.stderr)
    sys.exit(1)

unexpected_crates = [name for name in crates if name not in publishable]
if unexpected_crates:
    print(f"non-publishable or unknown crates in CRATES: {unexpected_crates}", file=sys.stderr)
    sys.exit(1)

order = {name: idx for idx, name in enumerate(crates)}
for name in crates:
    pkg = packages[name]
    for dep in pkg["dependencies"]:
        if dep["kind"] == "dev":
            continue
        dep_name = dep["name"]
        if dep_name not in packages:
            continue
        if dep_name not in order:
            print(f"{name} depends on unpublishable workspace crate {dep_name}", file=sys.stderr)
            sys.exit(1)
        if dep_name not in internal_crates:
            print(f"internal workspace dependency {dep_name} (used by {name}) missing from INTERNAL_CRATES", file=sys.stderr)
            sys.exit(1)
        if dep["req"] == "*":
            print(f"{name} -> {dep_name} lacks an explicit version requirement for crates.io publishing", file=sys.stderr)
            sys.exit(1)
        if order[dep_name] >= order[name]:
            print(f"publish order violation: {name} appears before its dependency {dep_name}", file=sys.stderr)
            sys.exit(1)

print(f"ok publish graph ({len(crates)} crates in topological order)")
' "$CRATES" "$INTERNAL_CRATES"
}

run_check() {
  check_publish_graph

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
    while IFS= read -r line; do
      if [ -z "$line" ]; then
        continue
      fi
      version="$(dependency_version "$dep" "$line")"
      if ! crate_version_indexed "$dep" "$version"; then
        echo "skipping package $crate until crates.io indexes $dep $version"
        return 1
      fi
    done < <(dependency_lines "$manifest" "$dep")
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
