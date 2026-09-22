# Protocol comparison

| Project | Browser-to-browser P2P | Structured P2P | Privacy layer | E2E encryption |
|---|---|---|---|---|
| **Rings** | Yes (WebRTC) | Yes (Chord) | Separate onion-circuit layer | Yes (opt-in E2E streams) |
| **libp2p** | Yes (WebRTC) | Optional (Kademlia DHT) | No built-in anonymity layer | Yes (peer connections, including circuit relays) |
| **aMule / eD2k / Kad** | No | Yes for Kad; no for eD2k | No anonymity layer | Protocol obfuscation only; no secure E2E guarantee |
| **Nostr** | No (client-to-relay) | No | No built-in network anonymity layer | Yes for encrypted messages (e.g. NIP-44); not public events |
| **Nym mixnet** | No direct P2P (browser clients use gateways) | No DHT overlay (layered mixnet) | Full mixnet path | Yes (between Nym clients) |
| **Tor** | No | No (relay network) | Yes (onion circuits) | Yes for onion services; HTTPS needed beyond an exit |

Browser P2P means the browser itself establishes a peer connection. Structured P2P
means a DHT-organized overlay. Privacy describes network-metadata protection;
E2E describes payload encryption between the stated endpoints. A full mixnet path
does not mean unconditional anonymity or protection beyond a network exit.

## Primary sources

- **Rings:** [security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts).
- **libp2p:** [WebRTC](https://libp2p.io/docs/webrtc/), [Kademlia DHT specification](https://github.com/libp2p/specs/blob/master/kad-dht/README.md), [encrypted circuit-relay connections](https://libp2p.io/docs/circuit-relay/).
- **aMule:** [eD2k/Kad](https://wiki.amule.org/wiki/FAQ_eD2k-Kademlia), [browser remote control](https://wiki.amule.org/wiki/AMuleWeb), [protocol obfuscation](https://wiki.amule.org/wiki/AMule_is_slow). Obfuscation targets protocol classification rather than a secure E2E messaging contract.
- **Nostr:** [NIP-01 client/relay protocol](https://github.com/nostr-protocol/nips/blob/master/01.md), [NIP-44 encrypted payloads](https://github.com/nostr-protocol/nips/blob/master/44.md).
- **Nym:** [mixnet design](https://nym.com/nym-whitepaper.pdf), [browser SDK](https://nym.com/blog/introducing-the-nym-sdk-powerful-privacy-served-directly-to-your-browser), [client-to-client encryption](https://nym.com/blog/nym-gateways-gateways-to-privacy).

- **Tor:** [relay network](https://community.torproject.org/relay/types-of-relays/), [onion-service E2E encryption](https://community.torproject.org/onion-services/overview/), [HTTPS and exit traffic](https://support.torproject.org/about-tor/security/https-encryption-and-tor/).

Sources checked September 22, 2026. Nym here means the mixnet, and its E2E endpoints
are Nym clients; a connection beyond an exit needs its own application encryption.
libp2p's secure peer connections do not automatically encrypt application messages
end to end across a pubsub forwarding path. Rings' plain overlay does not inherit
the anonymity properties of its separate onion circuits.
