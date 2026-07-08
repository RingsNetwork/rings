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
