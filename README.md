<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/logo/rings-white.svg">
  <img alt="Rings Network" src="assets/logo/rings.svg" width="128" height="128">
</picture>

# Rings Network

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

The threat model and the contract of each layer are documented in
[SECURITY.md](./SECURITY.md). DID authentication proves key control; it is not, by
itself, Sybil or eclipse resistance for permissionless public membership. The overlay
routes and, after an E2E handshake, encrypts; it does not hide who is talking to whom.
That is the job of the privacy layer, the onion loops in `crates/node/src/onion`.

At the application layer, Rings gives developers a namespace-scoped protocol runtime:
write a pure state machine, attach an interpreter shell, and run it over a decentralized
overlay. Built-in protocols cover peer service relay and echo; the roadmap extends
both the network layer and the privacy layer.

## Where Rings fits

✅ supported · ❌ not provided. Qualifications are listed below the table.

| Network | Browser P2P | Structured P2P | Privacy layer | E2E encryption |
|---|:---:|:---:|:---:|:---:|
| **Rings** | ✅ | ✅ Chord | ✅ Separate layer | ✅¹ |
| **libp2p** | ✅ | ✅ Kademlia² | ❌ | ✅³ |
| **aMule / Kad** | ❌ | ✅ Kademlia | ❌ | ❌⁴ |
| **Nostr** | ❌ | ❌ | ❌ | ✅⁵ |
| **Nym mixnet** | ❌ | ❌ | ✅ Full mixnet path | ✅⁶ |
| **Tor** | ❌ | ❌ Relay network | ✅ Onion circuits | ✅⁷ |
| **I2P** | ❌ | ✅ netDb² | ✅ Tunnel network | ✅⁶ |
| **WebTorrent (browser)** | ✅ | ❌ | ❌ | ✅³ |

Browser P2P means the browser itself connects as a peer; a browser UI or gateway
client does not qualify. Structured P2P includes DHT-based discovery; it does not
imply that application traffic is routed through the DHT. Privacy means network
metadata protection, separate from payload encryption.

1. Rings E2E streams require the E2E handshake; plain overlay messages are not E2E-encrypted.
2. libp2p offers an optional Kademlia DHT. I2P uses a Kademlia-based netDb for discovery; messages travel through tunnels.
3. Encrypted peer connections: libp2p includes circuit-relayed connections; browser WebTorrent uses WebRTC/DTLS. This does not make published content private or add E2E to pubsub forwarding.
4. aMule protocol obfuscation is not a secure E2E guarantee. This row covers Kad; eD2k uses indexing servers.
5. Nostr supports encrypted messages, such as NIP-44 payloads; public events are not encrypted.
6. Between Nym clients or I2P destinations. Traffic beyond an exit/outproxy needs application encryption.
7. Tor provides E2E for onion services; ordinary websites need HTTPS beyond the exit. A privacy layer is not an unconditional anonymity guarantee.

[Primary sources](./docs/src/README.md#primary-sources) ·
[Rings security model](./SECURITY.md#layer-contracts)

## Reading paths

- **Run a peer:** [Installation](#installation), then [node operations](./docs/src/cli.md).
- **Build an application:** [Examples](#examples) and [Extending Rings](#extending-rings).
- **Evaluate the design:** [Protocol comparison](#where-rings-fits), [Architecture](#architecture), and [Security model](./SECURITY.md).
- **Read the research:** [Papers and build instructions](./papers/README.md), including DRanking and finger convergence.

## Whitepaper

The canonical protocol paper is maintained in this repository:

- [Rings whitepaper PDF](./papers/rings.pdf)
- [LaTeX source](./papers/rings.tex)
- [Paper assets and build notes](./papers/README.md)
- [DRanking paper](./papers/dranking.pdf): proposed verifiable ranking and admission;
  the current [provisional service receipts](./docs/src/advanced-topic/dranking-service-receipts.md)
  implement only an evidence-collection slice, not the full ranking or admission protocol.
- [Finger-convergence specification](./papers/finger-convergence.pdf): range-proved
  finger convergence and its assumptions.

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

### Privacy layer

Onion loops in [`crates/node/src/onion`](./crates/node/src/onion) carry traffic over
direct edges as client-sealed loops of fixed-width Sphinx cells, with constant-rate link
emission that substitutes real cells for cover, paid admission, and fixed cell size
classes. A loop hides the route's hops from one another and hides the client from the
exit, whose reply returns through the loop. It does not hide the client from its first
hop, does not hide overlay membership, and does not hide each link's active/idle phase
from an observer that watches it. The plain overlay relay offers none of this: it minimizes what it leaks, and
the privacy layer is where privacy is provided. See
[the layer contracts](./SECURITY.md#layer-contracts).

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
user-facing Rings web surface for the landing page, the [guide](https://rings.rs/#guide),
the browser node console, onion proxy WorkBench, wallet login, SDP/HTTP connectivity,
topology, and custom messages.

## Examples

Runnable examples live in [`examples/`](./examples):

| Example | What it shows |
|---|---|
| [`native`](./examples/native) | A minimal native node registering a custom namespaced protocol |
| [`relay`](./examples/relay) | TCP & UDP tunnels to a peer's service over the overlay (`tcp.rs` / `udp.rs`) |
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

Proof systems use the same user-owned protocol boundary: register a private namespace, encode
versioned proof requests and results as application payloads, and perform proving or verification
in the protocol interpreter. Rings authenticates and routes the envelope but does not select,
execute, or maintain a proving backend.

## Resources

| Resource | Link | Notes |
|---|---|---|
| Rings Whitepaper | [PDF](./papers/rings.pdf), [LaTeX source](./papers/rings.tex), [citation](#whitepaper) | Canonical protocol paper |
| Security model | [SECURITY.md](./SECURITY.md) | Overlay assumptions, deployment models, Sybil boundary, and the communication-layer / privacy-layer contracts |
| Browser frontend | [`frontend`](./frontend) | Landing guide, web app, and extension workflow |
| Documentation | [rings.rs/docs](https://rings.rs/docs/), [source](./docs) | mdBook book, published with the site |
| Guide | [rings.rs/#guide](https://rings.rs/#guide) | One card per runtime with the first commands; the book is the reference |
| For AI agents | [rings.rs/llms.txt](https://rings.rs/llms.txt), [source](./llms.txt) | Project map for coding agents ([llms.txt](https://llmstxt.org/)), maintained with the code |
| Examples | [`examples/`](./examples) | Native, dweb, relay, and FFI examples |
| X (Twitter) | [@RingsNetworkio](https://x.com/RingsNetworkio) | Project announcements |

## Components

* core: DHT, swarm, DID routing, messages, and cryptographic identity primitives.

* node: Native daemon, browser/WASM provider, extension runtime, relay protocol, and FFI provider.

* rpc: Rings RPC shared types and the JSON-RPC client/handlers (over HTTP).

* derive: Rings macros, including `wasm_export` macro.

* transport: Native WebRTC transport and `web_sys`-based browser transport.

## Architecture

Rings separates peer connectivity, overlay routing, privacy loops, and application
protocols. Direct WebRTC connections can carry traffic without an application server;
bootstrap, signaling, and ICE infrastructure still matter, and a selected TURN relay
carries transport traffic. This does not establish permissionless Sybil resistance;
see [SECURITY.md](./SECURITY.md). Each layer maps to a crate or module:

```text
┌──────────────────────────────────────────────────────────────────────┐
│  Applications   dWeb · relay/tunnel · your own app                     │
├──────────────────────────────────────────────────────────────────────┤
│  Protocols      built-ins: relay (tcp/udp tunnels), echo —             │  node::extension::protocols
│  (namespaced)   plus any user Protocol, addressed by namespace         │
├──────────────────────────────────────────────────────────────────────┤
│  Extension      pure `Protocol::step` → `Effect` → `Interpret` shell   │  node::extension::ext
│  runtime        over a namespace-scoped `Scope` (send / self-inject)   │
├──────────────────────────────────────────────────────────────────────┤
│  Privacy        onion loops: Sphinx cells over direct edges,           │  crates/node/src/onion
│  (loops)        constant-rate cover, paid admission, exit registry     │
├──────────────────────────────────────────────────────────────────────┤
│  Overlay        Chord DHT: successor / finger tables, stabilization,   │  crates/core
│  (routing +     DID addressing, message relay, network_id isolation,   │
│  encryption)    E2E ElGamal to a DID after its handshake               │
├──────────────────────────────────────────────────────────────────────┤
│  Transport      direct WebRTC datachannels (native + browser/web_sys), │  crates/transport
│                 STUN / ICE / SDP NAT traversal                         │
├──────────────────────────────────────────────────────────────────────┤
│  Identity       DID + secp256k1 / secp256r1 / ed25519 / BLS / bip137   │  crates/core::ecc
└──────────────────────────────────────────────────────────────────────┘
```

- **Transport** establishes direct, peer-to-peer WebRTC datachannels — browser-to-browser
  included — using STUN/ICE/SDP for NAT traversal. Direct connectivity depends on the
  network; when a configured TURN relay is selected, that relay carries the data.
  Native nodes deployed behind cloud firewalls can bound ICE UDP gathering with
  `external_ip`, `webrtc_udp_port_min`, and `webrtc_udp_port_max`; for example,
  `49160..=49200` maps to an AWS security-group rule for `UDP 49160-49200`.
  Browser nodes still use the browser ICE stack, whose local UDP ports are not
  controlled by Rings.
- **Overlay** organizes peers into a Chord DHT and routes messages by DID; distinct overlays are
  isolated by `network_id`. Its contract is routing and confidentiality only: a payload can be
  encrypted to the destination's account key once the E2E handshake has supplied that key, but
  every hop sees the origin DID by signature and the destination DID by routing. The overlay
  minimizes what it leaks; it does not provide privacy.
- **Privacy** is the onion loop data plane: client-sealed loops of fixed-width Sphinx cells
  over direct edges, constant-rate link emission with real cells substituted for cover, paid
  admission, fixed cell size classes, and route selection from the onion-relay and onion-exit
  registries. Each position learns its predecessor and its successor and nothing else about the
  route. See [SECURITY.md](./SECURITY.md#layer-contracts).
- **Extension runtime** is a *functional core / imperative shell*: a protocol's state transition
  is pure (`step`), and all IO happens in its `Interpret` shell, which only ever receives a
  **namespace-scoped capability** (`Scope`). The core owns no global effect/command bus — adding
  a protocol never touches it, and a protocol cannot reach another namespace.
- **Protocols** are addressed by namespace. Built-ins include a **relay** that tunnels local
  TCP/UDP sockets to a peer's service across the overlay (server-less tunneling / peer exit), and
  an echo protocol used by examples and tests. Register your own with
  `provider.register_protocol(..)` (Rust) or `provider.on(namespace, ..)` (JS).

The **privacy layer** exists today as `crates/node/src/onion`; where it and the **network layer**
are heading — a fully server-less, sovereign network — is described in [ROADMAP.md](./ROADMAP.md).

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

## License

Rings is released under the GNU Affero General Public License, version 3.0 only
([AGPL-3.0-only](./LICENSE)). Works derived from it must be released under the same license,
and the AGPL's network clause extends that obligation to services that offer Rings over a
network: a product or hosted service built on Rings must publish its source.

A commercial license for use outside the AGPL's terms is available from Rings Network on
request. The Rings Network name, logo, and the hosted [rings.rs](https://rings.rs) service are
not covered by the software license.


## Standards and references

- [ICE: RFC 8445](https://www.rfc-editor.org/rfc/rfc8445) (obsoletes RFC 5245).
- [WebRTC IP address handling: RFC 8828](https://www.rfc-editor.org/rfc/rfc8828).
- [WebRTC data channels: RFC 8831](https://www.rfc-editor.org/rfc/rfc8831).
- [Data Channel Establishment Protocol: RFC 8832](https://www.rfc-editor.org/rfc/rfc8832).
- [Protocol comparison primary sources](./docs/src/README.md#primary-sources).
