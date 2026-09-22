# Protocol comparison sources

The [README capability matrix](https://github.com/RingsNetwork/rings#where-rings-fits)
compares browser P2P, structured P2P, privacy layers, and E2E encryption.

## Primary sources

- **Rings:** [security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md#layer-contracts).
- **libp2p:** [WebRTC](https://libp2p.io/docs/webrtc/), [Kademlia DHT specification](https://github.com/libp2p/specs/blob/master/kad-dht/README.md), [encrypted circuit-relay connections](https://libp2p.io/docs/circuit-relay/).
- **aMule:** [eD2k/Kad](https://wiki.amule.org/wiki/FAQ_eD2k-Kademlia), [browser remote control](https://wiki.amule.org/wiki/AMuleWeb), [protocol obfuscation](https://wiki.amule.org/wiki/AMule_is_slow). Obfuscation targets protocol classification rather than a secure E2E messaging contract.
- **Nostr:** [NIP-01 client/relay protocol](https://github.com/nostr-protocol/nips/blob/master/01.md), [NIP-44 encrypted payloads](https://github.com/nostr-protocol/nips/blob/master/44.md).
- **Nym:** [mixnet design](https://nym.com/nym-whitepaper.pdf), [browser SDK](https://nym.com/blog/introducing-the-nym-sdk-powerful-privacy-served-directly-to-your-browser), [client-to-client encryption](https://nym.com/blog/nym-gateways-gateways-to-privacy).

- **Tor:** [relay network](https://community.torproject.org/relay/types-of-relays/), [onion-service E2E encryption](https://community.torproject.org/onion-services/overview/), [HTTPS and exit traffic](https://support.torproject.org/about-tor/security/https-encryption-and-tor/).

- **I2P:** [Kademlia-based network database](https://i2p.net/en/docs/overview/network-database/), [tunnels and destination-to-destination encryption](https://i2p.net/en/docs/overview/intro/).
- **WebTorrent (browser):** [browser-to-browser WebRTC and tracker discovery](https://webtorrent.io/faq), [WebRTC data-channel encryption](https://www.rfc-editor.org/rfc/rfc8831). The row covers browser peers, not the native client's DHT support.

Sources checked September 22, 2026.
