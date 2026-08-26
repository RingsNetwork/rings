<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/logo/rings_network_red.png">
  <img alt="Rings Network" src="assets/logo/rings_network_black.svg">
</picture>

Rings Network
===============

[![rings-node](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings/actions/workflows/auto-release.yml)
[![cargo](https://img.shields.io/crates/v/rings-node.svg)](https://crates.io/crates/rings-node)
[![docs](https://docs.rs/rings-node/badge.svg)](https://docs.rs/rings-node/latest/rings_node/)
![GitHub](https://img.shields.io/github/license/RingsNetwork/rings)
[![Sponsor](https://img.shields.io/badge/Sponsor-RingsNetwork-ea4aaa?logo=githubsponsors)](https://github.com/sponsors/RingsNetwork)

**A peer-to-peer network for the sovereign age.**

Rings is a browser-native, structured peer-to-peer network for applications that need
their own network layer instead of a server-owned data path. Browser tabs and native
daemons can join the same overlay, discover peers by DID, and exchange messages over
direct WebRTC datachannels routed by a Chord DHT.

The current overlay threat model is documented in [SECURITY.md](./SECURITY.md).
DID authentication proves key control; it is not, by itself, Sybil or eclipse
resistance for permissionless public membership.

At the application layer, Rings gives developers a namespace-scoped protocol runtime:
write a pure state machine, attach an interpreter shell, and run it over a decentralized
overlay. Built-in protocols already cover peer service relay and fold-scheme zkSNARK
proving; the roadmap extends this into a fully server-less network layer and privacy
layer.

## Whitepaper

The canonical protocol paper is maintained in this repository:

- [Rings whitepaper PDF](./papers/rings.pdf)
- [LaTeX source](./papers/rings.tex)
- [Paper assets and build notes](./papers/)

If you cite Rings in academic or technical writing, use:

```bibtex
@misc{rings-network,
  author = {Ryan J. Kung},
  title = {Rings: A peer-to-peer network for sovereign age},
  year = {2023},
  month = feb,
  url = {https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf},
  note = {Repository-owned whitepaper and LaTeX source: https://github.com/RingsNetwork/rings/tree/master/papers}
}
```

## Features

### Browser-native peers

Rings runs in browsers through WebAssembly and `web_sys`, and on native hosts through
the same Rust node stack. WebRTC datachannels carry peer-to-peer traffic, including
browser-to-browser connections without an application server in the data path.

### DID identity and cryptography

Peers are addressed by decentralized identifiers backed by selectable signature
schemes, including secp256k1, secp256r1, ed25519, BLS, and bip137. This lets Rings
bridge browser, daemon, and wallet-oriented identity workflows without binding the
network to one key system. See [the threat model](./SECURITY.md#did-identity) for
the boundary between DID authentication and Sybil resistance.

### Structured peer routing

The overlay uses a Chord DHT for successor/finger-table routing, DID lookup, message
relay, stabilization, and `network_id` isolation. Independent overlays stay separate
while retaining deterministic routing behavior. Chord routing assumes an acceptable
membership model; see [the overlay threat model](./SECURITY.md#chord-routing).

### Protocol runtime

Application protocols are namespace-scoped. A protocol's `step` function stays pure,
and all side effects are performed by its `Interpret` shell through a scoped capability.
That keeps protocol logic extensible without adding a global effect bus to the core.

## Installation

You can install rings-node either from Cargo or from source.

### From Cargo

Install the `rings` CLI from crates.io:

```sh
cargo install rings-node
```

### From source

Install the CLI from a local checkout:

```sh
git clone git@github.com:RingsNetwork/rings.git
cd ./rings
cargo install --path crates/node
```

### User-level daemon

Initialize the default `~/.rings/config.yaml`, then install and start Rings as a
user-level service:

```sh
rings init
rings daemon start
```

The daemon commands are:

```sh
rings daemon start
rings daemon stop
rings daemon status
rings daemon restart
```

On macOS, Rings installs `~/Library/LaunchAgents/io.ringsnetwork.node.plist`. On
Linux, it installs `$XDG_CONFIG_HOME/systemd/user/rings-node.service` when
`XDG_CONFIG_HOME` is absolute, and otherwise falls back to
`$HOME/.config/systemd/user/rings-node.service`.
`start` enables login startup. `stop` and `restart` preserve the existing login
startup setting; stopping an enabled service therefore leaves it registered for
the next login, while restarting a disabled service does not silently enable it.
If restoring a temporarily changed launchd setting fails, `restart` exits with an
explicit error and reports that preservation could not be completed.

The service uses the directory where `rings daemon start` was run as its working
directory. This lets the daemon load the same `.env` file as a foreground
`rings run`. `CONFIG`, `LOG_LEVEL`, and `RUNTIME` from the installing shell or
that directory's `.env` file are parsed by the CLI and copied into the service
definition. Other shell variables are not copied into the definition; put
persistent node settings in the Rings configuration file or the captured `.env`
file. Run `daemon start` again after changing the executable path, configuration
path, working directory, log level, or runtime scheduler stored in the service
definition.
The captured working directory must continue to exist at the same path. Moving
or deleting it prevents the service manager from starting the node; run
`rings daemon start` again from a persistent directory to update the definition.
`daemon start` and `daemon restart` use a bounded sequence of manager
observations; this is a CLI responsiveness budget, not a wall-clock guarantee,
because each manager command can take additional time. launchd `throttled`
means a delayed spawn has been scheduled, so Rings keeps observing it within
that budget. A Rings-rendered systemd unit uses a five-second `RestartSec`,
deliberately longer than the observation budget: an `auto-restart` snapshot is
therefore returned as `restarting` instead of waiting long enough for the next
activation to hide the crash loop. If the final state is not running, the
command exits non-zero with that state, and `rings daemon status` shows any
later transition. A detached systemd unit with no local definition can
disappear during restart and then be reported as not installed. On macOS, both
`daemon start` and `daemon stop` also use a separate bounded poll to confirm
that any loaded job has been unloaded. Exhausting that unload poll is a hard
`ServiceDidNotUnload` error rather than a successful status report.

On macOS, standard output is written to `~/.rings/logs/daemon.log` and standard
error to `~/.rings/logs/daemon.error.log`. On Linux, inspect logs with:

```sh
journalctl --user -u rings-node.service
```

Linux daemon commands require an active per-user systemd manager and user bus.
To keep the user service running after logout, an administrator can enable
lingering for that account:

```sh
loginctl enable-linger "$USER"
```

There is intentionally no daemon uninstall command. To remove the registration,
stop it first and remove the platform definition manually:

```sh
# macOS
rings daemon stop
launchctl enable "gui/$(id -u)/io.ringsnetwork.node"
rm ~/Library/LaunchAgents/io.ringsnetwork.node.plist

# Linux
systemctl --user disable --now rings-node.service
case "${XDG_CONFIG_HOME:-}" in
  /*) rings_config_home=$XDG_CONFIG_HOME ;;
  *) rings_config_home=$HOME/.config ;;
esac
rm "$rings_config_home/systemd/user/rings-node.service"
systemctl --user daemon-reload
```

Linux must disable the unit to remove the default-target symlink created by
`systemctl enable`. On macOS, `rings daemon stop` unloads the job, while the
plist and launchd's per-user enable/disable override are independent. The
`launchctl enable` step normalizes a stale disabled override for a future manual
`launchctl bootstrap`; `rings daemon start` already performs this step itself.
launchd provides no label-scoped command to delete that override database
entry, so it may retain a harmless enabled record after manual removal.

### Build for WebAssembly

Build the browser provider with Cargo and `wasm-bindgen`:

```sh
cargo build -p rings-node --release --target wasm32-unknown-unknown --no-default-features --features browser
wasm-bindgen --out-dir pkg --target web ./target/wasm32-unknown-unknown/release/rings_node.wasm
```

Or build with `wasm-pack`:

```sh
wasm-pack build --scope ringsnetwork -t web crates/node --no-default-features --features browser,console_error_panic_hook
```

## Usage

```sh
rings --help
```

## Frontend

The browser and extension frontend lives in [`frontend`](./frontend). It is the
user-facing Rings web surface for the landing guide, browser node console,
onion proxy WorkBench, wallet login, SDP/HTTP connectivity, topology, proof
workbench, and custom messages.

## Examples

Runnable examples live in [`examples/`](./examples):

| Example | What it shows |
|---|---|
| [`native`](./examples/native) | A minimal native node registering a custom namespaced protocol |
| [`relay`](./examples/relay) | TCP & UDP tunnels to a peer's service over the overlay (`tcp.rs` / `udp.rs`) |
| [`snark`](./examples/snark) | Fold-scheme zkSNARK proving / verification |
| [`proof-demo`](./examples/proof-demo) | A browser zk-proof app (Yew / Trunk) |
| [`dweb`](./examples/dweb) | A decentralized-web app (Yew / Trunk) |
| [`ffi`](./examples/ffi) | Driving a node over the C FFI |

## Extending Rings

A protocol is a **pure** state machine; all IO lives in its interpreter shell, which can only
act within its own namespace. Inbound overlay messages are routed to a protocol by namespace.

```rust
// Register a pure Protocol + its Interpret shell, then route inbound envelopes to it.
provider.register_protocol(Echo, EchoShell)?;
provider.set_backend()?;

// Built-in relay: tunnel a local socket to a peer's service over the overlay — no server.
let relay = RelayHandle::install(&provider.extensions())?;
relay.register_tcp_service("web".into(), "example.com:80".parse()?).await?; // server side
relay.open_tcp_tunnel(local_addr, peer_did, "web".into()).await?;          // client side
```

In the browser a protocol can be a JS handler instead: `provider.on(namespace, initialState,
handler)`. See [`examples/relay`](./examples/relay) and
[`crates/node/src/extension`](./crates/node/src/extension).

## Resources

| Resource | Link | Notes |
|---|---|---|
| Rings Whitepaper | [PDF](./papers/rings.pdf), [LaTeX source](./papers/rings.tex), [citation](#whitepaper) | Canonical protocol paper |
| Security model | [SECURITY.md](./SECURITY.md) | Overlay assumptions, deployment models, and Sybil boundary |
| Browser frontend | [`frontend`](./frontend) | Landing guide, web app, and extension workflow |
| Examples | [`examples/`](./examples) | Native, dweb, proof, relay, snark, and FFI examples |

## Components

* core: DHT, swarm, DID routing, messages, and cryptographic identity primitives.

* node: Native daemon, browser/WASM provider, extension runtime, relay protocol, and FFI provider.

* rpc: Rings RPC shared types and the JSON-RPC client/handlers (over HTTP).

* derive: Rings macros, including `wasm_export` macro.

* transport: Native WebRTC transport and `web_sys`-based browser transport.

* snark: Fold-scheme zkSNARK proving and verification protocol.

## Architecture

Rings is layered so that **every layer is decentralized — there is no server in the data
path**. This is a data-path topology statement, not a permissionless Sybil-resistance
claim; see [SECURITY.md](./SECURITY.md). Each layer maps directly to a crate/module:

```text
┌──────────────────────────────────────────────────────────────────────┐
│  Applications   dWeb · zk-proof demo · relay/tunnel · your own app     │
├──────────────────────────────────────────────────────────────────────┤
│  Protocols      built-ins: relay (tcp/udp tunnels), SNARK, echo —      │  node::extension::protocols
│  (namespaced)   plus any user Protocol, addressed by namespace         │  crates/snark
├──────────────────────────────────────────────────────────────────────┤
│  Extension      pure `Protocol::step` → `Effect` → `Interpret` shell   │  node::extension::ext
│  runtime        over a namespace-scoped `Scope` (send / self-inject)   │
├──────────────────────────────────────────────────────────────────────┤
│  Overlay        Chord DHT: successor / finger tables, stabilization,   │  crates/core
│  (routing)      DID addressing, message relay, network_id isolation    │
├──────────────────────────────────────────────────────────────────────┤
│  Transport      direct WebRTC datachannels (native + browser/web_sys), │  crates/transport
│                 STUN / ICE / SDP NAT traversal                         │
├──────────────────────────────────────────────────────────────────────┤
│  Identity       DID + secp256k1 / secp256r1 / ed25519 / BLS / bip137   │  crates/core::ecc
└──────────────────────────────────────────────────────────────────────┘
```

- **Transport** establishes direct, peer-to-peer WebRTC datachannels — browser-to-browser
  included — using STUN/ICE/SDP for NAT traversal, so traffic never transits a central server.
  Native nodes deployed behind cloud firewalls can bound ICE UDP gathering with
  `external_ip`, `webrtc_udp_port_min`, and `webrtc_udp_port_max`; for example,
  `49160..=49200` maps to an AWS security-group rule for `UDP 49160-49200`.
  Browser nodes still use the browser ICE stack, whose local UDP ports are not
  controlled by Rings.
- **Overlay** organizes peers into a Chord DHT and routes messages by DID; distinct overlays are
  isolated by `network_id`.
- **Extension runtime** is a *functional core / imperative shell*: a protocol's state transition
  is pure (`step`), and all IO happens in its `Interpret` shell, which only ever receives a
  **namespace-scoped capability** (`Scope`). The core owns no global effect/command bus — adding
  a protocol never touches it, and a protocol cannot reach another namespace.
- **Protocols** are addressed by namespace. Built-ins include a **relay** that tunnels local
  TCP/UDP sockets to a peer's service across the overlay (server-less tunneling / peer exit), and
  **SNARK** (fold-scheme zkSNARK) proving/verification. Register your own with
  `provider.register_protocol(..)` (Rust) or `provider.on(namespace, ..)` (JS).

Where this is heading — a fully server-less, sovereign **network layer** and **privacy layer** —
is described in [ROADMAP.md](./ROADMAP.md).

## Contributing

We welcome contributions to rings-node!

If you have a bug report or feature request, please open an issue on GitHub.

If you'd like to contribute code, please follow these steps:

```text
    Fork the repository on GitHub.
    Create a new branch for your changes.
    Make your changes and commit them with descriptive commit messages.
    Push your changes to your fork.
    Create a pull request from your branch to the main repository.
```

We'll review your pull request as soon as we can, and we appreciate your contributions!


## Ref:

1. <https://datatracker.ietf.org/doc/html/rfc5245>

2. <https://datatracker.ietf.org/doc/html/draft-ietf-rtcweb-ip-handling-01>

3. <https://datatracker.ietf.org/doc/html/rfc8831>

4. <https://datatracker.ietf.org/doc/html/rfc8832>
