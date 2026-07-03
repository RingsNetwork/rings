# Examples

This directory contains runnable examples and integration tests for the example
surfaces.

## Test Commands

Run the workspace examples that are Cargo workspace members:

```bash
cargo test -p rings-native-example
cargo test -p rings-relay-example
cargo test -p rings-snark-example
```

`rings-native-example` includes the example extension protocol and a direct
ElGamal E2E stream round trip. `rings-relay-example` includes deterministic
local TCP/UDP echo tests in addition to overlay relay tests.

Run the standalone wasm/Yew browser frontend from its own workspace:

```bash
cd examples/frontend && cargo check --target wasm32-unknown-unknown
cd examples/frontend && cargo test --release --target wasm32-unknown-unknown
cd examples/frontend && trunk serve --release true
```

Run the standalone dweb and proof wasm/Yew demos from their own workspaces:

```bash
cd examples/dweb && wasm-pack test --headless --chrome
cd examples/proof-demo && wasm-pack test --headless --chrome
```

Run the FFI Python integration tests after building the cdylib:

```bash
cargo build -p rings-node --features ffi
python -m pip install web3 cffi pytest
RINGS_FFI_REQUIRE_LIBRARY=1 pytest examples/ffi/tests
```

`crates/node/include/rings.h` is the crate-owned FFI header consumed by the
Python example. The Python tests create two FFI providers and connect them with
the raw offer/answer/accept RPC path, so the FFI example is not only a nodeInfo
smoke test.
