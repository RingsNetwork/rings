# How handshake works

Rings Network decouples transport link negotiation from `rings-core` through the generalized
`HandshakeStep` interface in `crates/transport` (`0` to `MAX_HANDSHAKE_STEPS = 8` binary steps,
up to `DEFAULT_MAX_HANDSHAKE_STEP_BYTES = 4096` per step), so Native nodes, Browser nodes (via
WebAssembly), and custom transports participate as equal peers.

For the built-in WebRTC backend, peers use Interactive Connectivity Establishment (ICE) and
UDP/TCP TURN traversal to establish direct, DTLS-encrypted datachannels across a 2-step Session
Description Protocol (SDP) offer/answer exchange. Every handshake payload is framed with the
active wire header (`"RINGS\0" ‖ WIRE_VERSION (6)`) and mismatched versions fail fast with error
`210` before decoding.

## Three connection paths

- **HTTP Seed Endpoint:** A joining node dials a Native node's public external JSON-RPC listener
  (`nodeDid`, `answerOffer`, and `acceptAnswer` are public without Bearer authentication) for
  fast direct onboarding (`rings connect node <url>` or `provider.connect_peer_via_http(url)`).
- **Chord Overlay Routing:** Once connected to at least one peer, a node can connect to any DID
  across the overlay (`rings connect did <did>`). `ConnectNodeSend` records traversed relay links so
  `ConnectNodeReport` retraces the exact path back, with DID tie-breaking resolving concurrent
  crossing offers cleanly.
- **Manual Out-of-Band Exchange:** Two nodes without mutual HTTP reachability exchange encoded
  signed offers and answers directly (`rings connect offer` / `answer` / `accept`), or via
  `createOffer`, `answerOffer`, and `acceptAnswer` over JSON-RPC.

## Related chapters

- [Exchange SDP](exchange-sdp.md) — step-by-step manual two-node handshake walkthrough
- [Transport Abstraction](transport.md) — `0`–`8` step `HandshakeStep` state machine and custom backends
- [Wire Protocol & LZ4 Compression](wire-protocol.md) — `"RINGS\0" ‖ WIRE_VERSION` frame headers
