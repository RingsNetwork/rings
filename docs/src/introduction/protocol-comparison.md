# Rings and other peer-to-peer systems

Rings provides a structured application overlay: browser and native peers exchange
DID-addressed messages over WebRTC connections organized by Chord. Its extension
runtime hosts application protocols, while separate onion circuits provide a more
limited view of a route to each relay.

This comparison concerns architecture, not measured throughput, latency, adoption,
or security rankings. aMule is a client for eD2k and Kad, libp2p is a networking
stack, Nostr is an event protocol, and Nym is a privacy network. None is a drop-in
wire-protocol replacement for Rings. Sources were checked on September 22, 2026.

## Networking and application model

| System | What applications build on | Discovery and delivery |
|---|---|---|
| Rings | Namespaced protocols with a pure state transition and an interpreter for effects | Chord lookup and multi-hop message routing by DID; WebRTC links between adjacent peers |
| libp2p | Composable transports, secure channels, stream protocols, discovery, and pubsub | Applications choose mechanisms such as Kademlia, rendezvous, or gossip; a DHT lookup is distinct from application-message delivery |
| aMule / eD2k / Kad | File search, source discovery, queues, and file transfer | eD2k uses indexing servers; Kad uses a Kademlia-based DHT. File data is exchanged between clients |
| Nostr | Signed events, filters, subscriptions, and event-kind conventions | Clients publish to and query chosen relays; NIP-01 does not require a global DHT or relay-to-relay consensus |
| Nym mixnet | Sending traffic through a mix network | Gateways connect clients to a multi-hop mixnet; mix nodes transform and delay packets |

See [libp2p's protocol documentation][libp2p], the [aMule network FAQ][amule],
[NIP-01][nip01], and the [Nym whitepaper][nym].

## Browser peers and connectivity

**Rings:** a browser runs the Wasm node and participates in the overlay, rather
than only controlling a remote daemon. Initial connectivity still needs a way to
reach a peer and exchange SDP. ICE may use STUN, and the transport accepts TURN
configuration. When ICE selects a TURN relay, traffic traverses that relay; direct
connectivity is not guaranteed across all NATs and firewalls. See
[connection setup](connect-rings-network.md) and [SDP exchange](../advanced-topic/exchange-sdp.md).

**libp2p:** browser participation is supported. Its WebRTC transport can use a
circuit relay for browser-to-browser signaling, then establish a direct data
channel; WebRTC Direct supports browser-to-public-node connectivity. Transport
support varies across implementations. Browser support is therefore not a unique
Rings advantage. The difference is Rings' integrated Chord/DID/runtime design versus
libp2p's selectable components. See the [WebRTC documentation][webrtc].

**aMule:** the peer is the native aMule client or daemon. Its [web interface][amuleweb]
remotely controls that client; the browser itself does not become a Kad peer.

**Nostr:** browser clients use WebSocket connections to relays. Relays accept,
store according to event-kind rules and policy, and deliver matching events.
A client can use several relays, but those relays remain part of event delivery.
See [NIP-01][nip01].

**Nym:** clients enter through gateways; running an application client is a
different role from operating a mix node. The comparison here is to the mixnet,
not every mode or product sold under the Nym name. See the [network design][nym].

## Identity, privacy, and admission

| System | Identity and security boundary |
|---|---|
| Rings | DID authentication proves key control. Plain overlay hops see origin and destination DIDs; payload confidentiality requires the E2E handshake and encrypted stream. Onion circuits are separate, and Sybil-permissive membership limits route security |
| libp2p | Peer IDs identify cryptographic peers; secure channels protect connections. Admission policy and anonymity require additional mechanisms chosen by the application |
| aMule / eD2k / Kad | File identifiers, source discovery, and upload queues serve file distribution; they do not supply a general anonymous application transport |
| Nostr | Public keys and signatures authenticate events. NIP-01 alone neither encrypts event content nor hides client connections from relays; other NIPs may add application-level encryption |
| Nym mixnet | Layered Sphinx packets, mixing delays, and cover traffic are designed to resist traffic analysis. Their protection depends on the whitepaper's adversary and network assumptions |

Rings' cover cells and onion encryption do not establish equivalence to Nym's
mixnet threat model. Rings does not claim resistance to an observer watching every
link, nor Sybil-resistant public membership. Read the
[Rings security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md)
for current contracts. This is a difference in goals and mechanisms, not a measured
anonymity comparison.

The [DRanking paper](https://github.com/RingsNetwork/rings/blob/master/papers/dranking.pdf)
proposes trust-weighted ranking and admission. The implementation currently exposes
[provisional service receipts](../advanced-topic/dranking-service-receipts.md);
those receipts do not implement the paper's finalized ledger, global trust weights,
committee selection, or admission policy.

## Choosing a starting point

- Consider **Rings** for an application whose browser and native nodes should share
  a structured, DID-addressed overlay and protocol runtime, within its documented
  membership assumptions.
- Consider **libp2p** when selecting and composing networking components is central
  to the application's design.
- Consider **aMule / eD2k / Kad** when the requirement is participation in those
  file-sharing networks.
- Consider **Nostr** when the application model is signed events distributed by
  independently operated relays.
- Consider **Nym's mixnet** when traffic-analysis resistance is a central requirement
  and the application can accommodate its latency and cover-traffic overhead.

These systems can also occupy different layers of a larger application. An adapter
or integration still needs its own protocol, threat-model, and performance review.

## Primary sources

- [libp2p documentation][libp2p]: networking components and protocol references.
- [libp2p WebRTC][webrtc]: browser connectivity and signaling.
- [aMule eD2k/Kademlia FAQ][amule]: indexing, discovery, and transfers.
- [aMule web interface][amuleweb]: remote client control.
- [Nostr NIP-01][nip01]: signed events and client/relay communication.
- [Nym whitepaper][nym]: mixnet architecture and security assumptions.

[libp2p]: https://libp2p.io/docs/
[webrtc]: https://libp2p.io/docs/webrtc/
[amule]: https://wiki.amule.org/wiki/FAQ_eD2k-Kademlia
[amuleweb]: https://wiki.amule.org/wiki/AMuleWeb
[nip01]: https://github.com/nostr-protocol/nips/blob/master/01.md
[nym]: https://nym.com/nym-whitepaper.pdf
