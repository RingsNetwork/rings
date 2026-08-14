# CI security gates

The `QACI` workflow contains the blocking pull-request security gates. These
checks are intentionally narrower than the full build matrix so failures point at
one security boundary at a time.

## Dependency policy

Status: blocking PR gate on Ubuntu.

Purpose: fail the PR when the lockfile introduces a RustSec advisory, an
unreviewed license, a wildcard dependency, or a dependency from an unapproved
registry or git source.

Local reproduction:

```sh
cargo install cargo-deny --version 0.20.2 --locked
cargo deny --locked check advisories licenses bans sources
```

Reviewed exceptions live in `deny.toml`. Advisory exceptions must use the
structured form with an issue reference and an expiry date. Git sources must be
listed explicitly under `sources.allow-git`; do not rely on broad organization
allow-lists for new sources.

## Miri core invariants

Status: blocking PR gate on Ubuntu.

Purpose: run Miri with strict provenance over core modules that do not require
native sockets, wall-clock access during interpretation, or long elliptic-curve
searches.

Local reproduction:

```sh
rustup toolchain install nightly-2026-07-02 --profile minimal
rustup component add miri rust-src --toolchain nightly-2026-07-02
cargo +nightly-2026-07-02 miri setup
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2026-07-02 miri test -q -p rings-core --lib lifecycle::tests
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2026-07-02 miri test -q -p rings-core --lib dht::did::tests
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly-2026-07-02 miri test -q -p rings-core --lib dht::virtual_node::tests
```

Do not replace these filters with the full core suite without first validating
runtime and duration locally. Some otherwise valid core tests use wall-clock
operations or expensive crypto paths that are poor fits for blocking Miri CI.

## FFI ASan and LSan

Status: blocking PR gate on Ubuntu.

Purpose: exercise the native FFI lifecycle under AddressSanitizer with leak
detection enabled. The job fails if the filtered FFI tests execute zero tests, so
a renamed or removed lifecycle test cannot silently pass.

Local reproduction on Linux:

```sh
rustup toolchain install nightly-2026-07-02 --profile minimal
RUSTFLAGS=-Zsanitizer=address \
ASAN_OPTIONS=detect_leaks=1:strict_string_checks=1:check_initialization_order=1 \
LSAN_OPTIONS=print_suppressions=0 \
cargo +nightly-2026-07-02 test -p rings-node --features ffi ffi_ -- --nocapture
```

This sanitizer job is Linux-only because the GitHub-hosted Linux runner provides
the expected sanitizer runtime and leak detector behavior. macOS still runs the
ordinary FFI build and Python integration tests.

## Pinned tools

Workflow actions are pinned to reviewed commit SHAs with the original tag kept as
an inline comment. Tool installers use exact versions instead of mutable install
scripts or major-version aliases:

- Rust stable: `1.97.0`
- Rust nightly/Miri/rustfmt: `nightly-2026-07-02`
- `wasm-bindgen`: `0.2.121`
- `wasm-pack`: `0.15.0`
- Node.js: `20.20.2`
- Python: `3.11.16`
- `cargo-deny`: `0.20.2`
- `trunk`: `0.21.14`
- `taplo-cli`: `0.10.0`
