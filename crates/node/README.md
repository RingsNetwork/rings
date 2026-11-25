<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings-white.svg">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings.svg" width="128" height="128">
</picture>

Rings Node (The node service of Rings Network)
===============

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg?version=1.0.0-pre.11)](https://docs.rs/rings-node/1.0.0-pre.11/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings)


Rings is a structured peer-to-peer network implementation using WebRTC, Chord algorithm, and full WebAssembly (WASM) support.

For protocol details, see the repository-owned [Rings Whitepaper](../../papers/rings.pdf).
For security assumptions, supported deployment models, and the Sybil-resistance
boundary, see the repository [threat model](../../SECURITY.md).

In addition to Chord overlay routing, Storage, and PubSub, `rings-node` exposes Sphinx onion routing (`onion`), generic `onion::udp` exits (`- udp`), Kleisli `invoke` pipelines (`OnionInvokeOffer`, `OnionInvokePipeline`), the DNS relay (`dns_relay`), and the native TUN packet gateway (`rings-gateway`). For an interactive terminal console against a running node, see [`rings-tui`](../tui/README.md); for TURN/onion-ticket services, see [`rings-turn-service`](../turn-service/README.md).

## Installation

You can install `rings-node` either from Cargo or from source.

### From Cargo

To install `rings-node` from Cargo, build with the Rust release pinned in the repository's
`rust-toolchain.toml` (the workspace denies warnings, so a newer compiler can fail the build):

```sh
cargo +1.97.0 install --locked rings-node
```

### From Source

To install `rings-node` from source, follow these steps:

```sh
git clone https://github.com/RingsNetwork/rings.git
cd ./rings
cargo install --path crates/node
```

### Build for WebAssembly

To build `rings-node` for WebAssembly from the workspace root, run:

```sh
cargo build -p rings-node --lib --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build with `wasm-pack`:

```sh
wasm-pack build crates/node --scope ringsnetwork -t web --release --no-default-features --features browser_default
```


## Usage

```sh
rings [--log-level <LOG_LEVEL>] [--runtime <multi-thread|current-thread>] <COMMAND>
```

### Global Options

- `--log-level <LOG_LEVEL>` (env `LOG_LEVEL`): sets tracing log verbosity (default `info`).
- `--runtime <RUNTIME>` (env `RUNTIME`): chooses the Tokio runtime scheduler (`multi-thread` default or `current-thread`).
- `-h, --help`: displays usage information.
- `-V, --version`: displays version information for `rings-node`.

### Subcommands

- `init`: creates a default configuration file (`~/.rings/config.yaml` unless `--location` says otherwise) and generates key material and an owner-only `api-token` file. The generated file states every section explicitly, including a complete `gateway:` section with `enabled: false` (see [Native gateway](#native-gateway)).
- `new-delegation`: generates or renews a delegated session signing key.
- `run`: runs the foreground `rings-node` daemon (accepts `-c, --config <FILE>`, defaulting to `~/.rings/config.yaml`, plus `--gateway`, listener, ICE, storage, onion, and DNS overrides).
- `pubsub`: subscribes to a topic stream over the node's local API.
- `connect`: connects to peers (`node`, `did`, `seed`) or runs manual 3-step WebRTC handshakes (`offer`, `answer`, `accept`).
- `peer`: lists connected peers or disconnects a peer (`list`, `disconnect`).
- `send`: sends a namespaced backend message to a peer (`message`).
- `service`: registers or looks up named services on the DHT (`register`, `lookup`).
- `inspect`: shows swarm state including transport table, successors, predecessor, and finger table.

Subcommands that run or talk to a node (`run`, `pubsub`, `connect`, `peer`, `send`, `service`, `inspect`) accept `-c, --config <FILE>` (env `CONFIG`, default `~/.rings/config.yaml`); see the [CLI reference](https://rings.rs/docs/cli.html) for full flags.

### Control API security

`rings init` creates an owner-only `api-token` file next to the YAML configuration. The token is
presented as an `Authorization: Bearer ...` header; the `rings` CLI reads the token file
automatically. Which requests demand it depends on the listener and, on the external listener,
on the JSON-RPC method:

| Listener | Method | Authorization |
|---|---|---|
| internal (`internal_api_port`, default 50000) | every JSON-RPC method, `/status`, `/gateway/status` | Bearer required |
| external (`external_api_addr`, default 127.0.0.1:50001) | `nodeDid`, `answerOffer`, `acceptAnswer` | public |
| external (`external_api_addr`, default 127.0.0.1:50001) | all other JSON-RPC methods (e.g. `nodeInfo`, `lookupOnlineNodes`, `lookupOnionExits`), `/status` | Bearer required |

The public methods cover the HTTP handshake (`nodeDid` identifies the peer DID to dial, `answerOffer` admits a signed offer bound to that peer DID, and `acceptAnswer` is guarded by a live pending `transport_id` and signed answer payload), so a seed can admit arbitrary peers without sharing its bearer token. `answerOffer` resolves crossing offers by returning `answer` when the caller prevails or `counterOffer` when a pending overlay offer from an older peer prevails; in the latter case the caller answers it and submits the reply via `acceptAnswer`. A batch is authorized by its strictest member: a batch that mixes a public and a gated method requires the token. Every JSON-RPC request, public or gated, must use `Content-Type: application/json`.

Browser origins are denied by default. Add exact origins to `api_allowed_origins` in the YAML
configuration or repeat `--api-allowed-origin` when starting the node. Wildcard origins are not
accepted. A non-loopback `external_api_addr` additionally requires
`allow_remote_external_api: true` or `--allow-remote-external-api`.

If the external API is explicitly bound to a non-loopback address, terminate TLS in front of it;
plain HTTP exposes bearer tokens to anyone able to observe that network path.

Connecting to a remote peer needs no token by default, because its handshake is public. An
operator who fronts the external listener with a proxy that demands the token can still be
dialled: `rings connect node` accepts `--remote-api-token-file`, and seed entries may include an
optional `api_token` field.

### Native gateway

`rings init` writes a complete `gateway:` section so the generated file is the one place an
operator edits. The section is inert as written: the gateway starts only under an explicit
`enabled: true` or `rings run --gateway`, and a hand-written section that omits `enabled` is also
inert. `rings run` on the generated file therefore creates no TUN device.

```yaml
gateway:
  enabled: false
  plan:
    addresses:
    - 100.64.0.1/32          # RFC 6598 shared address space; collides with neither LAN nor VPN ranges
    included_routes: []      # nothing is captured until a destination prefix is listed here
    mtu: 1280                # the IPv6 minimum link MTU, which every underlay carries
  max_flows: 1024
  flow_idle_timeout: 300
  tcp_buffer_bytes: 65536
  interface_name: null
  route_ledger_path: /home/operator/.rings/gateway-routes.json   # resolved from $HOME at init time
  unix_helper_socket: /home/operator/.rings/gateway-helper.sock  # must match the helper's --socket
  wintun_dll_path: null
  status_refresh_secs: 2
```

`rings run --gateway` enables the section for that run without editing the file. On a config
written before this section existed, `--gateway` fails and prints the section to append. The
[rings-gateway](../gateway/README.md) README covers the capture contract, the Unix helper, and
the capabilities a TUN device needs; none of that is granted by the section itself.
