# Embed via C FFI

`rings-node` exports a small C ABI so a host that cannot call Rust directly — Python, Swift,
Kotlin, or any language with a C foreign-function interface — runs a full node in its own
process. The ABI is declared in
[`crates/node/include/rings.h`](https://github.com/RingsNetwork/rings/blob/master/crates/node/include/rings.h),
the crate-owned C header for the `ffi` feature. It is deliberately narrow: two provider
constructors, one request entry point that carries the JSON-RPC and provider-local RPC API, and
the lifecycle/free helpers those calls need.

## Build the library

```bash
cargo build -p rings-node --features ffi
```

The library lands in `target/debug/` as `librings_node.dylib` (macOS), `librings_node.so`
(Linux), or `rings_node.dll` (Windows); add `--release` for `target/release/`.

## The ABI

| Function | Role |
|---|---|
| `rings_node_init_logging(level)` | Installs the logger once; `level` is `Debug`, `Info`, `Warn`, `Error`, or `Trace`. |
| `rings_node_new_provider_with_callback(network_id, ice_server, stabilize_interval, account, account_type, storage_path, signer)` | Creates a provider keeping its stores under `storage_path`, and returns its handle, or `NULL` when creation fails. |
| `rings_node_new_provider_with_serialized_config(serialized_config, storage_path)` | Creates a provider from a YAML or JSON `ProcessorConfig` (including `network: { kind, params }`) and opens its stores under `storage_path`, returning `NULL` on failure. |
| `rings_node_listen(provider)` | Starts message listening and Chord stabilization. |
| `rings_node_request(provider, method, params)` | Calls any method of the [JSON-RPC API](jsonrpc.md) or [provider-local RPC extensions](jsonrpc.md#provider-local-rpc-methods) (`takeE2eEvents`, `dispatchDropCounters`, `signingHorizon`, `renewDelegateeKey`, `lookupRegistry`, `onionInvoke`) with a JSON `params` string; returns the reply as a string, or `NULL` when validation or execution fails. |
| `rings_node_string_free(value)` | Releases a string returned by `rings_node_request`. |
| `rings_node_provider_destroy(provider)` | Stops the listener and every task of the node, closes its connections, flushes its measurements, and releases every store under its `storage_path`, so a provider created again on that path in this process opens them; then destroys the provider, and the handle is invalid afterwards. |

The constructor takes the same values `config.yaml` gives a native node: the `network_id`, an
ICE server list in `[turn|stun]://<host>:<port>;...` form, the stabilization interval in
seconds, the account encoded for its `account_type`, and its `account_type` (`eip191`,
`secp256k1`, `bip137`, `ed25519`, `secp256r1`, or `bls12-381`). The signer is a callback
`size_t signer(const char *proof, char *out, size_t out_cap)` that writes the account's exact
signature bytes over `proof` (65 for `eip191`/`secp256k1`/`bip137`, 64 for
`ed25519`/`secp256r1`, 96 for `bls12-381`, never exceeding `out_cap`) into `out` and returns the
number of bytes written (`0` on failure); the node calls it to sign its session and never sees
the private key.

`storage_path` is a directory the provider owns. Under it the provider opens the same stores a
native node opens beside its data store, each at its authority: `data` (the DHT data store),
`transaction-replay` (the replay windows, authoritative), `relay-inbox` (held mail, retained),
`dht-removals` (the data topics' uncollected removals, retained), `onion-entry-guards` (a durable
cache), and `measure` with `provisional-evidence`. Pass the same
path on every run of the same node to preserve crash-consistent `Authoritative` replay windows
(`.p` presence witnesses + group commit), pinned onion entry guards, and held relay mail across
restarts. Passing a fresh path initializes a fresh node state.

The provider owns its stores exclusively while it lives. Each store takes an exclusive lock on
its own directory at open (a `.lock` file, `flock` on unix and `LockFileEx` on Windows) and
holds it until the provider is destroyed. A second provider, or a native daemon, given a
store this provider holds fails to open it, so both `rings_node_new_provider_with_callback` and
`rings_node_new_provider_with_serialized_config` return `NULL` instead of interleaving writes.
The lock is per store, so providers with distinct roots never conflict. A file system that offers
no such lock fails the open.

## Lifecycle

```c
rings_node_init_logging(Info);

struct ProviderHandle *provider = rings_node_new_provider_with_callback(
    1, "stun://stun.l.google.com:19302", 15, account, "eip191", "/var/lib/myapp/rings", signer);
rings_node_listen(provider);

char *reply = rings_node_request(provider, "connectPeerViaHttp",
                                 "{\"url\": \"https://node.rings.rs\"}");
/* ... read the JSON reply ... */
rings_node_string_free(reply);

rings_node_provider_destroy(provider);
```

The ownership rules are the ones the header states, and every wrapper in the repository
follows them:

- A handle is released exactly once with `rings_node_provider_destroy`, never used afterwards,
  and never destroyed concurrently with another call on it.
- Every non-`NULL` string from `rings_node_request` is released exactly once with
  `rings_node_string_free`; passing `NULL` is a no-op.
- The signer callback stays alive for as long as the provider does: the host retains it.

## Wrappers

[`examples/ffi`](https://github.com/RingsNetwork/rings/tree/master/examples/ffi) holds three
wrappers over this ABI with the same desktop flow — node identity, offer/answer exchange, peer
connection polling, end-to-end handshake and messaging, and event polling:

- `rings.py`: a `cffi` binding that parses `rings.h` and loads the library for the host
  platform. Its tests run against a built library:

  ```bash
  python -m pip install web3 cffi pytest
  RINGS_FFI_REQUIRE_LIBRARY=1 pytest examples/ffi/tests
  ```

- `swift/`: a `RingsRuntime(libraryPath:)` wrapper that loads the library dynamically, with a
  smoke test that also verifies every required symbol when given the real library:

  ```bash
  bash examples/ffi/swift/test.sh target/debug/librings_node.dylib
  ```

- `kotlin/`: the same flow over JNA.

Because `librings_node` exposes a self-contained, portable C ABI with zero global OS coupling,
desktop and mobile host applications (including iOS `NetworkExtension` and Android `VpnService`
hosts) can embed it directly inside their own application lifecycle.
