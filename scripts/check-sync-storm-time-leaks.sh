#!/usr/bin/env bash
set -euo pipefail

readonly strict_paths=(
  crates/core/src/simulation.rs
  crates/core/src/simulation
  crates/core/src/tests/default/test_sync_storm
)
readonly forbidden='chrono::Utc::now|SystemTime::now|std::time::SystemTime|std::time::Instant|web_time::Instant|Uuid::new_v4|uuid::Uuid::new_v4|thread_rng|rand::random|futures_timer::Delay|tokio::time::sleep|tokio::spawn|Handle::spawn|spawn_local'

if command -v rg >/dev/null 2>&1; then
  forbidden_matches() {
    rg --line-number --glob '*.rs' "$forbidden" "${strict_paths[@]}"
  }
  has_fixed_string() {
    rg --quiet --fixed-strings "$2" "$1"
  }
else
  forbidden_matches() {
    grep --recursive --line-number --include='*.rs' --extended-regexp \
      "$forbidden" "${strict_paths[@]}"
  }
  has_fixed_string() {
    grep --quiet --fixed-strings "$2" "$1"
  }
fi

if forbidden_matches; then
  echo "sync-storm simulation reached a direct time, UUID, RNG, sleep, or spawn source" >&2
  exit 1
fi

readonly required_boundaries=(
  'crates/core/src/utils/time.rs:crate::simulation::epoch_ms_override'
  'crates/core/src/utils/id.rs:crate::simulation::next_uuid_override'
  'crates/core/src/utils/time.rs:tokio::time::sleep'
  'crates/transport/src/connections/dummy/delay.rs:controlled_random'
  'crates/core/src/simulation.rs:verify_effect_boundary'
  'crates/core/src/simulation/spawn.rs:Handle::try_current'
  'crates/core/src/simulation/spawn.rs:runtime.spawn'
)

for boundary in "${required_boundaries[@]}"; do
  file=${boundary%%:*}
  pattern=${boundary#*:}
  if ! has_fixed_string "$file" "$pattern"; then
    echo "missing deterministic effect boundary $pattern in $file" >&2
    exit 1
  fi
done

echo "sync-storm simulation has no direct time, UUID, RNG, sleep, or spawn leaks"
