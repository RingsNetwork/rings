# Issue #787: gateway and webview decisions

Baseline: `1dbdd5100d7c21f71d719a1b2879b97d9e82b5fd`, also the remote master observed
on 2026-09-22. The issue had no comments at inspection. This work excludes transport,
RPC cleanup, node wire changes, measure and the core outbound scheduler.

| Candidate and provenance | Decision and replacement evidence |
| --- | --- |
| Gateway degraded lifecycle/events (#697) | Removed. `GatewayStatus::from_state` already projects active lifecycle and exit availability to health. Keep `GatewayHealth::Degraded`; test loss, unknown availability and recovery without changing packet admission. |
| `TcpStack.owned_handles` (#697) | Removed. Private `admit_flow` inserts a TCP socket and its endpoint together; `release_socket` removes both. Endpoint lookup is the membership proof for typed smoltcp access. Tests cover repeated release, access after release, slot reuse, timeout, reset and half-close. |
| Captured to TargetBound (#697) | Collapsed at flow capture. The captured `FlowId` already carries the immutable target; the runtime previously emitted BindTarget immediately with no intervening IO. Opening/establishment ordering remains tested. |
| Repeated GatewayConfig validation (#697) | One borrowed immutable validation proof at runtime construction, shared by component constructors. Standalone public server/TCP constructors validate at their own boundaries. Removed the redundant node runner check; runner still constructs the checked runtime before platform setup. Keep independent FlowTable capacity and TunnelControl plan checks because those public entry points can run without a runtime. |
| Runtime status accessor (#697) | Removed in favor of the existing shared status handle snapshot; all test callers migrated. |
| TeardownFailure error accessor (#697) | Removed; consuming `into_parts` retains the cleanup lease and error together. Retry ownership behavior is unchanged. |
| TcpSegment FIN/RST/payload length (#697) | Removed duplicated metadata. The original validated bytes are still fed to smoltcp, which owns FIN/RST and payload processing. Reset tests now inspect the actual packet. |
| Discarded packet/flow rejection reasons (#684/#697) | Runtime dispatch emits structured debug tracing with the typed reason. Packet bodies and flow addresses are excluded. |
| Webview stripped-header denylist (#666, later privacy allowlist #715) | Replaced with the shared request allowlist. Simple deletion was unsafe because normalization adds Origin/Accept-Encoding and cookie preparation adds Cookie before preflight. These gateway-owned headers stay excluded from author preflight. Raw CORS fixtures now use forwarded Cache-Control rather than discarded X-Requested-With. |
| Duplicate source carriers (#666) | Keep trusted full `source_target`, derive `url::Origin` inside the crate. All constructors, direct DTO construction, frontend conversion, browser fixtures, cookie and CORS consumers migrated together. The trusted frame source remains host-supplied; diagnostic path/query remain intact, upstream Origin contains neither. |
| Third response-body limit (#666/#702) | Retained with evidence: `finish_response` rewrites URLs and can inject bootstrap code after the post-send check. Existing HTML and CSS tests construct sub-limit input whose rewritten body exceeds the limit and require rejection. Both post-send checks (preflight and actual response) remain. |

No candidate in this scope was already absent at the baseline. The mixed issue row's
`controlled::{discard, inspect}` and `handle_admitted_frame` refer to the separate core
transport work and are not modified here.

## Validation

Executed successfully on macOS Apple Silicon:

- `cargo test -p rings-gateway -p rings-webview --all-targets --all-features`: 198 passed,
  including the Chromium/Playwright gateway fixture; two privileged gateway tests ignored.
- `cargo clippy -p rings-gateway -p rings-webview --all-targets --all-features -- -D warnings`.
- `cargo check -p rings-node --no-default-features --features node --tests`.
- Frontend `cargo check` and `cargo clippy --all-targets -- -D warnings`, both targeting
  `wasm32-unknown-unknown`.
- Gateway `cargo check --all-targets` targeting `x86_64-pc-windows-msvc` and
  `aarch64-unknown-linux-gnu`. These are compile checks, not OS runtime tests.
- Repository nightly formatter, `taplo format --check` for the changed manifest, prose
  `typos`, `git diff --check`, and `mdbook build docs`.

Privileged native TUN/route and helper tests were not executed. They require host network
privileges; Linux namespace and Windows Wintun runtime validation require those operating
systems. A successful cross-compile or an ignored test is not evidence of successful host
network cleanup. The existing public-network node gateway tests are separately ignored by
default and are not counted in the 198 passing tests.


The frontend WASM test binary compiled, but browser execution was blocked before test
results: the default Firefox WebDriver returned HTTP 500; an explicit ChromeDriver retry
returned HTTP 404. Neither attempt is reported as a passing frontend test run. The separate
Playwright Chromium webview fixture did execute and pass. Frontend lockfile normalization
and unrelated baseline formatting churn produced by validation were discarded.

The node gateway test binary also built successfully with `--no-default-features --features
node --lib processor::tests::test_gateway`; its one matching macOS public-network test was
ignored (zero executed). The final raw-packet FIN/payload preservation regression was rerun
and passed, and final gateway/webview clippy passed after that assertion change.
