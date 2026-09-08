<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings_network_red.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/rings/master/assets/logo/rings_network_black.svg">
</picture>

Rings Node (The node service of Rings Network)
===============

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings)


Rings is a structured peer-to-peer network implementation using WebRTC, Chord algorithm, and full WebAssembly (WASM) support.

For protocol details, see the repository-owned [Rings Whitepaper](../../papers/rings.pdf).
For security assumptions, supported deployment models, and the Sybil-resistance
boundary, see the repository [threat model](../../SECURITY.md).

## Installation

You can install rings-node either from Cargo or from source.

### from cargo

To install rings-node from Cargo, run the following command:

```sh
cargo install rings-node
```

### from source

To install rings-node from source, follow these steps:

```sh
git clone git@github.com:RingsNetwork/rings.git
cd ./rings
cargo install --path crates/node
```

### Build for WebAssembly


To build Rings Network for WebAssembly, run the following commands:

```sh
cargo build --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build with `wasm-pack`

```sh
wasm-pack build --scope ringsnetwork -t web --no-default-features --features browser --features console_error_panic_hook
```


## Usage

```sh
rings <command> [options]
```

### Commands

- `help`: displays the usage information.
- `init`: creates a default configuration file, `~/.rings/config.yaml` unless `--location` says otherwise. This file can be edited to customize the behavior of the rings-node daemon. The generated file states every section explicitly, including a complete `gateway:` section with `enabled: false` (see [Native gateway](#native-gateway)).
- `run`: runs the rings-node daemon. This command starts the daemon process, which will validate transactions, maintain the blockchain, and participate in consensus to earn rewards. By default, the daemon will use the "config.toml" file in the current directory for configuration. Use the "-c" or "--config" option to specify a custom configuration file.

### Options

- `-c, --config <FILE>`: specifies a custom configuration file to use instead of the default "config.toml". The configuration file is used to specify the network configuration, account settings, and other parameters that control the behavior of the rings-node daemon.
- `-h, --help`: displays the usage information.
- `-V, --version`: displays the version information for rings-node.

### Control API security

`rings init` creates an owner-only `api-token` file next to the YAML configuration. The token is
presented as an `Authorization: Bearer ...` header; the `rings` CLI reads the token file
automatically. Which requests demand it depends on the listener and, on the external listener,
on the JSON-RPC method:

| Listener | Method | Authorization |
|---|---|---|
| internal (`internal_api_port`, default 50000) | every method, WebSocket, `/status`, `/gateway/status` | Bearer required |
| external (`external_api_addr`, default 127.0.0.1:50001) | `nodeDid`, `answerOffer` | public |
| external (`external_api_addr`, default 127.0.0.1:50001) | `nodeInfo`, `lookupOnlineNodes`, `lookupOnionExits`, `/status` | Bearer required |

The public methods are the two halves of the HTTP handshake, whose offer is already bound to the
peer DID by its signature, so a seed can admit arbitrary peers without sharing its token. A batch
is authorized by its strictest member: a batch that mixes a public and a gated method requires
the token. Every JSON-RPC request, public or gated, must use `Content-Type: application/json`.

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
  onion_service: tcp
  onion_hop_count: 0
  onion_allow_short_paths: false
```

`rings run --gateway` enables the section for that run without editing the file. On a config
written before this section existed, `--gateway` fails and prints the section to append. The
[rings-gateway](../gateway/README.md) README covers the capture contract, the Unix helper, and
the capabilities a TUN device needs; none of that is granted by the section itself.
