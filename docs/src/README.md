# Start Here

<img class="rings-mark" src="assets/rings.svg" alt="" width="56" height="56">

**Rings is a peer-to-peer network for the sovereign age.** Browser tabs and native daemons join
one overlay, find each other by DID, and exchange messages over direct WebRTC datachannels
routed by a Chord DHT. Direct connections do not need an application server in the data
path. Bootstrap and signaling establish the first links; if ICE selects a configured TURN
relay, that relay carries the transport traffic.

The overlay is the communication layer: it routes by DID and, once two peers have completed
the E2E handshake, encrypts to a DID. It does not hide who is talking to whom. Privacy is the
job of the privacy layer, the onion circuits that bound what each relay can learn to its
predecessor and its successor; the contract of each layer is drawn in the
[security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts).

On top of the overlay, Rings gives applications a namespace-scoped protocol runtime. A
protocol is a pure state machine; its interpreter shell performs the side effects, and only
within its own namespace. Built-in protocols cover peer service relay and echo; yours are
registered under a namespace of their own.

Rings is written in Rust and compiles to native binaries and to WebAssembly, so the same node
runs in a terminal, in a browser tab, and inside a host that speaks C. This book is the
reference for all three; the [Guide](https://rings.rs/#guide) on the site is its short form.

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

### Primary sources

<details>
<summary>Sources for the comparison</summary>

- **Rings:** [security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts).
- **libp2p:** [WebRTC](https://libp2p.io/docs/webrtc/), [Kademlia DHT specification](https://github.com/libp2p/specs/blob/master/kad-dht/README.md), [encrypted circuit-relay connections](https://libp2p.io/docs/circuit-relay/).
- **aMule:** [eD2k/Kad](https://wiki.amule.org/wiki/FAQ_eD2k-Kademlia), [browser remote control](https://wiki.amule.org/wiki/AMuleWeb), [protocol obfuscation](https://wiki.amule.org/wiki/AMule_is_slow). Obfuscation targets protocol classification rather than a secure E2E messaging contract.
- **Nostr:** [NIP-01 client/relay protocol](https://github.com/nostr-protocol/nips/blob/master/01.md), [NIP-44 encrypted payloads](https://github.com/nostr-protocol/nips/blob/master/44.md).
- **Nym:** [mixnet design](https://nym.com/nym-whitepaper.pdf), [browser SDK](https://nym.com/blog/introducing-the-nym-sdk-powerful-privacy-served-directly-to-your-browser), [client-to-client encryption](https://nym.com/blog/nym-gateways-gateways-to-privacy).
- **Tor:** [relay network](https://community.torproject.org/relay/types-of-relays/), [onion-service E2E encryption](https://community.torproject.org/onion-services/overview/), [HTTPS and exit traffic](https://support.torproject.org/about-tor/security/https-encryption-and-tor/).
- **I2P:** [Kademlia-based network database](https://i2p.net/en/docs/overview/network-database/), [tunnels and destination-to-destination encryption](https://i2p.net/en/docs/overview/intro/).
- **WebTorrent (browser):** [browser-to-browser WebRTC and tracker discovery](https://webtorrent.io/faq), [WebRTC data-channel encryption](https://www.rfc-editor.org/rfc/rfc8831). The row covers browser peers, not the native client's DHT support.

Sources checked September 22, 2026.

</details>

## Choose a runtime

| Runtime | Start with |
|---|---|
| Native node | [Install](install-a-native-node.md), then [Host a Native Node](host-a-native-node.md) and [Operate a Node from the CLI](cli.md) |
| Browser (Wasm) node | [Build for Wasm](build-for-wasm.md) and [Browser Node and Extension](browser-node.md) |
| Another language | [Embed via C FFI](ffi.md) |
| An AI coding agent | [llms.txt](llms.md) |

If you are new to Rings, start with a native node: it is the shortest path to a running peer
and to the JSON-RPC [API](jsonrpc.md) every runtime shares.

## How it works

- [Architecture](advanced-topic/architecture.md): the layers, from transport to application.
- [How handshake works](advanced-topic/handshake.md) and [Exchange SDP](advanced-topic/exchange-sdp.md): how two peers open a datachannel.
- [DHT - Network Layer](advanced-topic/chord.md): routing and lookup on the Chord ring.
- [Account Abstraction](advanced-topic/account-abstraction.md): DIDs and the signature schemes behind them.
- [config.yaml](advanced-topic/config.yaml.md): every section of a native node's configuration.

## Beyond the book

- [Rings whitepaper](https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf): the protocol paper.
- [Security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md): the assumptions, deployment models, the boundary between DID authentication and Sybil resistance, and the contracts of the communication layer and the privacy layer. Read it before deploying.
- [Roadmap](https://github.com/RingsNetwork/rings/blob/master/ROADMAP.md): where the network layer and the privacy layer are heading.
- [Source on GitHub](https://github.com/RingsNetwork/rings): the code, examples, and issues.
- [License](https://github.com/RingsNetwork/rings/blob/master/LICENSE): AGPL-3.0-only, so a product or hosted service built on Rings publishes its source; a commercial license for use outside those terms is available on request.
