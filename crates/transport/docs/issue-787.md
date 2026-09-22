# Issue #787 transport audit

Baseline: remote `master` at `1dbdd5100d7c21f71d719a1b2879b97d9e82b5fd`, fetched
on 2026-09-22. The issue body and repository-wide Rust callers were checked again.
All scoped candidates were still present; none was assumed complete from an old list.

| Candidate | Resolution and evidence |
| --- | --- |
| `ConnectionInterface::get_stats` | Removed from the trait, weak reference, three backends and test implementations. This was a debug string dump, separate from core measurement; there were no surviving callers. Removed browser dump helper and `RtcStatsReport` feature as part of the same API cutover. |
| `TransportInterface::connections`, `Pool::connections` | Removed the duplicate reference enumeration surface. Current callers enumerate `connection_ids` and resolve the current reference. A later cleanup retains that reference across await. |
| `TransportInterface::close_connection`, `Pool::safely_remove` | Removed CID-only retirement. Their replacement is the generation-pinned `close_connection_if_current` / `safely_remove_if_current`; existing callers and stale-reference pool tests already use this boundary. |
| `StatusPool` | Replaced its two implementations with backend-local inherent `all_ready` methods, preserving the same pool lock and all-channel readiness predicates. |
| `IceCredentialType::Oauth` | Removed the unsupported variant and native warning/browser token mapping. URL parsing always produces password credentials; native webrtc 0.17 cannot express OAuth. Explicit serialized OAuth now fails. Password representation is unchanged. |
| dummy `controlled::{discard, inspect}(index)` | Removed with the indexed state inspection helper. Simulation uses stable-sequence `discard_sequence` / `inspect_after`; indexed `deliver` and its queue removal remain because delivery tests use them. |
| second `handle_admitted_frame` check | Removed the peer-string disjunct. `admit_inbound_frame` constructs both private owner identity and peer lease from one immutable callback. Pointer identity remains the stronger boundary, tested against another callback both with the same CID and with a different CID. |
| native retire-fence guards | Retained with the timing evidence below. This is an explicit deferral of the proposed single-guard consolidation, not a claim that fence idempotence proves equivalent lifetimes. |
| `native_timeout_scheduler` | Retained: Tokio is optional and the backend-free build must work without any entered runtime. A plain-thread regression awaits the notifier with a non-Tokio executor. |
| `NativePhysicalCloseWitness` | Retained: logical fencing and physical close completion are distinct events. The cancelled-waiter regression observes completion after the waiter disappears. |

## Native guard ownership

`IrrevocableSendGuard` is bound to the permit before the first poll; a first-poll
panic releases the admission lock before the guard requests retirement. After a
pending first poll, this guard moves into the detached continuation. Consequently
it cannot synchronously fence admission when the caller is cancelled while that
continuation is still pending.

`run_send_with_retirement` therefore retains its caller-owned conditional fence
and physical retirement guard. On abandonment the fence runs before asynchronous
physical cleanup. On an observed failure it fences before awaiting close and
preserves the original send error. Accepted sends do not trigger this cleanup.

`run_irrevocable_send_with_timeout` has an unconditional continuation guard. It
fences before dropping pending send resources on timeout or panic. A permit guard
inside the send future does not provide that destruction-order guarantee. Tests
cover first-poll panic, revocable cancellation, accepted cancellation, caller
abandonment, detached completion, timeout fencing before Drop, and physical-close
completion. Collapsing these into only the permit guard would change those
ownership and timing obligations and needs a separate lifecycle redesign.

## Validation

All commands below ran from the workspace root except `wasm-pack`, which ran
from `crates/transport`.

| Verification | Result |
| --- | --- |
| `cargo test -p rings-transport --features native-webrtc` | 95 passed |
| `cargo test -p rings-transport --features dummy` | 83 passed |
| `cargo test -p rings-transport --no-default-features` | 71 passed |
| `wasm-pack test --headless --chrome --chromedriver /tmp/rings-transport-driver/chromedriver-mac-arm64/chromedriver --no-default-features --features web-sys-webrtc` | 5 browser tests passed |
| `cargo check -p rings-transport --no-default-features` | Passed; normal dependency tree contains no Tokio |
| `cargo check -p rings-core --features dummy` | Passed; includes native and dummy transport backends |
| `cargo check -p rings-core --target wasm32-unknown-unknown --no-default-features --features wasm` | Passed |
| Transport `cargo clippy --all-targets -- -D warnings` for native, dummy, no-default-features, and wasm web-sys configurations | All passed |
| `cargo +nightly fmt --all --check`, `git diff --check` | Passed |

The installed ChromeDriver 149 failed against Chrome 153 (HTTP 404 during driver
setup). The successful browser run used a temporary ChromeDriver 153.0.8010.52;
the installed driver was not changed. The no-default-features test build also
exposed two pre-existing unconditionally imported Tokio-test-only atomics; their
imports now have the matching feature gate. Dummy state tests now inspect through
the surviving stable-sequence API.

Native and browser features are mutually exclusive, so an all-features build is
not a valid verification configuration. No RPC DTO, gateway, webview, node,
measure, or core outbound mailbox implementation is changed.
