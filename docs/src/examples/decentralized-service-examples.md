# Decentralized Service Examples

The `examples/` directory contains five runnable suites covering custom extension protocols,
TCP/UDP overlay relays, decentralized web hosting, DID mail, and C ABI / multi-language FFI
bindings:

## 1. Minimal native node & extension protocol (`examples/native`)

Registers the `example` namespace (`Example` / `ExampleShell`) on a `Provider`, joins a running
seed daemon via HTTP (`connectPeerViaHttp`), and exchanges namespaced messages and E2E streams:

```sh
cargo run -p rings-native-example -- http://127.0.0.1:50001 <PEER_DID>
```

## 2. TCP and UDP relay tunnels (`examples/relay`)

For a local-service tunnel demo, the
[TCP/UDP relay examples](https://github.com/RingsNetwork/rings/tree/master/examples/relay) register
a named service and carry bytes between two Rings peers:

```sh
cargo run -p rings-relay-example --example rings-tcp-relay-example
cargo run -p rings-relay-example --example rings-udp-relay-example
cargo test -p rings-relay-example
```

## 3. Decentralized Web host & browser client (`examples/dweb`)

A standalone Yew/WebAssembly app registering the `dweb` protocol via `provider.on(...)`, hosting a
static page, and rendering peer-fetched HTML inside a sandboxed `<iframe srcdoc=... sandbox="">`:

```sh
cd examples/dweb
trunk serve
wasm-pack test --headless --chrome
```

## 4. DID mailbox & stateless SMTP MX gateway (`examples/mailbox`)

Provides `rings-mailbox` (SMTP receiver, local submission, IMAP4rev1 store behind either a DNS
relay `--did` or direct DHT gateway polling `--rings-config`) and `rings-mail-gateway` (stateless
MX SMTP gateway for `<did>@<domain>`):

```sh
cargo run -p rings-mailbox-example -- --rings-config ~/.rings/config.yaml
cargo run -p rings-mailbox-example --bin rings-mail-gateway -- --config ~/.rings/config.yaml --host mx.example.net
cargo test -p rings-mailbox-example
```

## 5. C FFI, Python, Swift, and Kotlin bindings (`examples/ffi`)

Builds the shared `librings_node` library (`crates/node/include/rings.h`) and runs desktop Python
(`cffi`), Swift (`dlopen`), and Kotlin (`JNA`) wrappers:

```sh
cargo build -p rings-node --features ffi
RINGS_FFI_REQUIRE_LIBRARY=1 pytest examples/ffi/tests
bash examples/ffi/swift/test.sh target/debug/librings_node.dylib
```

For anonymous traffic that hides endpoints, see
[Onion Routing & Kleisli Pipelines](../onion.md), [Native Gateway](../native-gateway.md), and
[Browser Node and Extension](../browser-node.md); direct overlay relays do not add onion hops.
