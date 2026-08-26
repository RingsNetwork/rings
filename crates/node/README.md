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
- `init`: creates the default `~/.rings/config.yaml` and session key.
- `new-session`: creates a new session secret key.
- `run`: runs the node in the foreground.
- `daemon start|stop|status|restart`: manages the node through the user-level macOS or Linux service manager.
- `pubsub`: publishes and subscribes to a topic.
- `connect node|did|seed`: connects to a remote peer.
- `peer list|disconnect`: inspects or disconnects peers.
- `send message`: sends a namespaced message to a peer.
- `service register|lookup`: registers or looks up a Rings network service.
- `inspect`: displays swarm routing and transport information.

### Options

- `-c, --config <FILE>`: uses a custom configuration file instead of `~/.rings/config.yaml` on commands that accept configuration.
- `-h, --help`: displays the usage information.
- `-V, --version`: displays the version information for rings-node.

`daemon start` records the current working directory so the managed process can
load the same `.env` file as `run`. `CONFIG`, `LOG_LEVEL`, and `RUNTIME` from the
installing shell or that `.env` file are copied into the service definition;
other shell variables are not copied. Persist node settings in the
configuration file or the captured `.env` file. macOS logs are stored under
`~/.rings/logs/`; Linux logs are available through
`journalctl --user -u rings-node.service`. See the repository
README for Linux lingering requirements and manual service removal. The captured
working directory must remain at the same path; rerun `daemon start` from a
persistent directory after moving or deleting it. `daemon stop` and `daemon
restart` preserve the current login-autostart setting unless the service manager
reports that its recovery step failed. Start and restart use a bounded sequence
of manager observations rather than a wall-clock deadline. launchd throttling
and systemd auto-restart remain pending because those states say another spawn
is scheduled. The command succeeds if the service runs and otherwise exits
non-zero with its last observed state. A detached systemd unit with no local
definition can disappear during restart and then be reported as not installed.
On macOS, start and stop also use a separate bounded poll to confirm unload.
