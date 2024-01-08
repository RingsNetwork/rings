<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>

Rings Node (The node service of Rings Network)
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings-node)


Rings is a structured peer-to-peer network implementation using WebRTC, Chord algorithm, and full WebAssembly (WASM) support.

For more details you can check our [Rings Whitepaper](https://raw.githubusercontent.com/RingsNetwork/whitepaper/master/rings.pdf).

You can also visit [Rings Network's homepage](https://ringsnetwork.io) to get more project info.

And you can get more document [here](https://rings.gitbook.io/).

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
git clone git@github.com:RingsNetwork/rings-node.git
cd ./rings-node
cargo install --path .
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
