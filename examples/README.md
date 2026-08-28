# Examples

This directory contains runnable examples and integration tests for the example
surfaces.

## Test Commands

Run the workspace examples that are Cargo workspace members:

```bash
cargo test -p rings-native-example
cargo test -p rings-relay-example
```

`rings-native-example` includes the example extension protocol and a direct
ElGamal E2E stream round trip. `rings-relay-example` includes deterministic
local TCP/UDP echo tests in addition to overlay relay tests.

The browser frontend moved to the repository-level [`frontend`](../frontend)
workspace because it also serves the landing guide and Chrome extension package:

```bash
cd ../frontend && cargo check --target wasm32-unknown-unknown
cd ../frontend && cargo test --release --target wasm32-unknown-unknown
cd ../frontend && trunk serve --release true
```

Run the standalone dweb wasm/Yew demo from its own workspace:

```bash
cd examples/dweb && wasm-pack test --headless --chrome
```

Run the FFI Python integration tests after building the cdylib:

```bash
cargo build -p rings-node --features ffi
python -m pip install web3 cffi pytest
RINGS_FFI_REQUIRE_LIBRARY=1 pytest examples/ffi/tests
```

`crates/node/include/rings.h` is the crate-owned FFI header consumed by the
Python, Swift, and Kotlin/JNA examples. The Python tests create two FFI providers
and connect them with the raw offer/answer/accept RPC path. The desktop Swift and
Kotlin smoke tests retain two distinct signer callbacks and verify response/free
and provider-destroy ownership against the same deterministic C fixture. See the
[desktop FFI guide](ffi/README.md) for their build and test commands.
