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

Rings is a structured peer-to-peer network that runs in the browser. Browser tabs and native
daemons join one Chord DHT overlay, are addressed by DIDs, and talk over direct WebRTC
datachannels, with no application server in the data path.

## Why Rings

- **A browser tab is a full peer.** It joins the DHT, routes and stores for others, and
  connects browser to browser, with no light client or gateway in between.
- **Privacy is built in.** Onion circuits with cover traffic and entry guards run natively
  and in the browser; the [hosted console](https://rings.rs/#node) browses the web through
  them from a tab.
- **Wallet keys are identities.** secp256k1 (MetaMask), ed25519 (Phantom), WebCrypto P-256,
  and bip137 keys share one overlay.
- **Protocols are pure state machines.** A protocol owns a namespace; its IO runs in a shell
  that cannot reach any other namespace.
- **Hardened.** Messages are signed, delivered at most once, and rate-limited per origin;
  connection, storage, and queue state is bounded. CI model-checks Chord rejoin, runs Miri and ASan/LSan, and feeds
  every wire decoder malformed input.

## Try it

- **Browser:** open [rings.rs/#node](https://rings.rs/#node). Nothing to install.
- **Native:** download `rings` from [Releases](https://github.com/RingsNetwork/rings/releases),
  then `rings init && rings run`. See [Installation](#installation).
- **Web app:** `npm install @ringsnetwork/rings-node`, then follow the
  [guide](https://rings.rs/#guide).

## Status

Pre-1.0, with a public overlay at the seed `node.rings.rs`. Until 1.0 the wire protocol is
not versioned, so a release that changes it is a network-wide upgrade (marked in the
[CHANGELOG](./CHANGELOG.md)). 1.0 freezes the protocol once connectivity and churn
handling are stable; DRanking ships as 2.0. See [ROADMAP.md](./ROADMAP.md).

The overlay authenticates and, after a handshake, encrypts; the privacy layer hides who
talks to whom. Making identities scarce for open membership is planned
([#780](https://github.com/RingsNetwork/rings/issues/780)). [SECURITY.md](./SECURITY.md)
has the full model.

## Where Rings fits

✅ supported · ❌ not provided. Browser P2P means the browser itself is a peer. Structured
P2P means DHT-based discovery. Privacy means metadata protection, not payload encryption.

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

1. After the E2E handshake; plain overlay messages are not E2E-encrypted.
2. libp2p's Kademlia DHT is optional. I2P's netDb is for discovery; messages travel through tunnels.
3. Encrypted connections only; published content and pubsub forwarding are not E2E.
4. aMule obfuscation is not secure E2E. eD2k uses indexing servers.
5. Encrypted messages such as NIP-44; public events are not encrypted.
6. Between Nym clients or I2P destinations; traffic past an exit needs its own encryption.
7. For onion services; ordinary sites need HTTPS past the exit.

[Primary sources](./docs/src/README.md#primary-sources)

## Architecture

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
│  Privacy        onion circuits: layered ElGamal-AEAD over direct       │  crates/node/src/onion
│  (circuits)     edges, fixed-batch cover + pacing, exit registry       │
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

The overlay routes by DID and every hop sees both endpoints; the privacy layer is where
endpoints are hidden ([layer contracts](./SECURITY.md#layer-contracts)). Connections are
direct unless ICE selects a configured TURN relay. Supporting crates: `rpc` (JSON-RPC),
`measure` (local peer credit), `gateway` (TUN over onion), `webview` (onion browsing),
`network-policy` (exit egress policy), `codec`, and `derive`.

## Build on Rings

A protocol is a pure state machine registered under a namespace; its interpreter shell
performs the IO.

```rust
provider.register_protocol(Echo, EchoShell)?;
provider.set_backend()?;

// Built-in relay: tunnel a local socket to a peer's service, no server.
let relay = RelayHandle::install(&provider.extensions())?;
relay.register_tcp_service("web".into(), "example.com:80".parse()?).await?; // server side
relay.open_tcp_tunnel(local_addr, peer_did, "web".into()).await?;          // client side
```

In the browser, a protocol can be a JS handler: `provider.on(namespace, initialState,
handler)`. Proof systems fit the same boundary: Rings routes and authenticates the
envelopes, and the protocol's shell runs the prover.

| Example | Shows |
|---|---|
| [`native`](./examples/native) | A native node with a custom protocol |
| [`relay`](./examples/relay) | TCP and UDP tunnels to a peer's service |
| [`dweb`](./examples/dweb) | A decentralized web app (Yew) |
| [`ffi`](./examples/ffi) | Driving a node over the C FFI |

## Installation

- **Prebuilt:** [Releases](https://github.com/RingsNetwork/rings/releases) ship `rings` for
  macOS (aarch64, x86_64), Linux (x86_64, static musl), and the Wasm package.
- **Cargo:** the workspace denies warnings, so use the toolchain pinned in
  [`rust-toolchain.toml`](./rust-toolchain.toml):

  ```sh
  cargo +1.97.0 install --locked rings-node
  ```

- **Source:** `git clone https://github.com/RingsNetwork/rings && cd rings && cargo install --path crates/node`
- **Wasm:** see [Build for Wasm](./docs/src/build-for-wasm.md).

## Documentation

| | |
|---|---|
| [rings.rs/docs](https://rings.rs/docs/) | The book: operating nodes, browser and FFI runtimes, protocol internals |
| [SECURITY.md](./SECURITY.md) | Vulnerability reporting, threat model, layer contracts |
| [ROADMAP.md](./ROADMAP.md) | Milestones and tracks |
| [llms.txt](./llms.txt) | Project map for AI agents |
| [`frontend`](./frontend) | Source of rings.rs and the browser extension |
| [@RingsNetworkio](https://x.com/RingsNetworkio) | Announcements |

## Whitepaper

- [Rings whitepaper](./papers/rings.pdf) ([LaTeX](./papers/rings.tex), [build notes](./papers/README.md))
- [DRanking](./papers/dranking.pdf): the ranking protocol planned for 2.0. Today's
  [provisional receipts](./docs/src/advanced-topic/dranking-service-receipts.md) only
  collect evidence.
- [Finger convergence](./papers/finger-convergence.pdf): range-proved finger convergence.

To cite Rings:

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

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## License

Rings is released under the GNU Affero General Public License, version 3.0 only
([AGPL-3.0-only](./LICENSE)). Works derived from it must be released under the same license,
and the AGPL's network clause extends that obligation to services that offer Rings over a
network: a product or hosted service built on Rings must publish its source.

A commercial license for use outside the AGPL's terms is available from Rings Network on
request. The Rings Network name, logo, and the hosted [rings.rs](https://rings.rs) service are
not covered by the software license.
