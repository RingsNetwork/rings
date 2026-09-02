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
- `init`: creates a default configuration file named "config.toml" in the current directory. This file can be edited to customize the behavior of the rings-node daemon.
- `run`: runs the rings-node daemon. This command starts the daemon process, which will validate transactions, maintain the blockchain, and participate in consensus to earn rewards. By default, the daemon will use the "config.toml" file in the current directory for configuration. Use the "-c" or "--config" option to specify a custom configuration file.

### Options

- `-c, --config <FILE>`: specifies a custom configuration file to use instead of the default "config.toml". The configuration file is used to specify the network configuration, account settings, and other parameters that control the behavior of the rings-node daemon.
- `-h, --help`: displays the usage information.
- `-V, --version`: displays the version information for rings-node.

### Control API security

`rings init` creates an owner-only `api-token` file next to the YAML configuration. The internal
and external JSON-RPC listeners, WebSocket endpoint, `/status`, and `/gateway/status` all require
that token as an `Authorization: Bearer ...` header. The `rings` CLI reads the token file
automatically. JSON-RPC requests must also use `Content-Type: application/json`.

Browser origins are denied by default. Add exact origins to `api_allowed_origins` in the YAML
configuration or repeat `--api-allowed-origin` when starting the node. Wildcard origins are not
accepted. A non-loopback `external_api_addr` additionally requires
`allow_remote_external_api: true` or `--allow-remote-external-api`.

If the external API is explicitly bound to a non-loopback address, terminate TLS in front of it;
plain HTTP exposes bearer tokens to anyone able to observe that network path.

Connecting to a protected remote peer requires its token. `rings connect node` accepts
`--remote-api-token-file`; seed entries may include an optional `api_token` field.
