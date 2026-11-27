# Build for Wasm

We offer several ways to use Wasm Rings, including:

1. Using Rings SDK
2. Using Rings Browser Extension
3. Build your custom-rings

### Rings SDK

To fetch the SDK from `npm`, just:

```sh
npm i @ringsnetwork/rings-node
```

If you want to use the latest version of Rings Network, you can also directly incorporate the latest version or a specific branch of Rings Network into your Node.js project by modifying `package.json`:

```json
"@ringsnetwork/rings-node": "https://github.com/RingsNetwork/rings.git#master"
```

As a developer, if you want to build local offline dependencies, use a git path in `package.json`:

```json
"@ringsnetwork/rings-node": "git+file:../rings#master"
```

Note: The reason for using a git path instead of a local path is that npm, after a certain version, no longer executes the prepare script in local path packages, and the package relies on the prepare script to help with wasm compilation.

### JavaScript / TypeScript SDK Reference (`Provider`)

- **Constructors**:
  - `new Provider(network_id, ice_servers, stabilize_interval, account, account_type, signer)` — mints a session delegation via `async function signer(proof: string): Promise<Uint8Array>` (`eip191`, `secp256k1`, `bip137`, `ed25519`, `secp256r1`, `bls12-381`) and opens exclusive Web-Locked IndexedDB stores.
  - `Provider.new_provider_with_config(config)` / `Provider.new_provider_with_storage(config, storage_name)` / `Provider.new_provider_with_serialized_config(config)` — build from a `ProcessorConfig` or YAML/JSON string.
  - `Provider.new_https_exit_instance(network_id, ice_servers, stabilize_interval, account, account_type, signer, allowed_targets, denied_targets)` — create a browser node advertising an `https` onion exit (targets must satisfy browser egress rules).
  - `Provider.new_provider_with_mail(config, storage_name, new BrowserMailOptions(), on_mail)` — enable [DID mail reception](mail-gateway.md#receiving-in-a-browser).
- **Lifecycle & locks**:
  - `const listener = provider.listen(); await listener.started();` starts swarm and stabilization loops (`listener.stop()`, `listener.is_stopped()`, `listener.task()`).
  - Opening browser stores acquires an exclusive Web Lock (`rings-node/<storage_name>`, `ifAvailable: true`), so a second tab targeting the same `storage_name` fails with `BrowserStorageLocked` (`823`).
  - `await provider.leave()` performs acknowledged relay-carrier handoff and broadcasts authenticated `Leave` (backed by `r`-member chain custody if a browser tab closes abruptly); `await provider.close()` stops tasks, leaves, flushes storage, and releases the Web Lock.
- **Networking, protocols & onion**:
  - `provider.address` (getter), `connect_peer_via_http(url)`, `connect_peer_via_http_with_token(url, token)`, `connect_with_address(addr, addr_type)`, `disconnect(addr, addr_type)`, `get_node_info()`, `get_peer_measurement(addr, addr_type)`, `lookup_service(name)`.
  - `provider.on(namespace, initial_state, handler)` registers a pure `(ctx, event) -> { state, effects }` extension protocol; `send_message(destination, namespace, Uint8Array)` dispatches to it.
  - `provider.onion_https_proxy()` returns a `BrowserOnionProxy` (`route(authority)`, `request(url, init)`).
  - `provider.lookup_registry(topic)` and `provider.onion_invoke(request)` evaluate DHT registries and 1-stage Kleisli `invoke` loops.
  - `provider.request(method, params)` executes any method from the [JSON-RPC & local RPC reference](jsonrpc.md).
  - `provider.signing_horizon()`, `renew_delegatee_key(delegatee_key)`, `renew_with_signer(account, account_type, signer)`, `take_e2e_events()`.

### Rings Browser Extension

The browser extension is the repository's frontend packaged as a Chrome Manifest V3
extension; [Browser Node and Extension](browser-node.md) covers the hosted console and the
packaging steps.

### Custom Build

To build `rings-node` for WebAssembly from the repository root, select the package explicitly so
workspace examples do not re-enable native features:

```sh
cargo build -p rings-node --lib --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build the npm package layout with `wasm-pack` by passing the `crates/node` package directory (matching `npm run wasm_pack` in root `package.json`):

```sh
wasm-pack build crates/node --release --scope ringsnetwork -t web --no-default-features --features browser_default
```

For core-only development, `cargo wasm` and `cargo test-wasm` select `rings-core`
with its `wasm` feature and no default features. Both aliases work from the
repository root or `crates/core`. The root `.cargo/config.toml` also supplies the
WASM test runner and timeout to both locations.
