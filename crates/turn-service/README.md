# rings-turn-service

External overlay glue connecting `rings-turn` (`RFC 5766` TURN server and unlinkable `SessionTicket` credentials) to `rings-node`'s generic foreground daemon, DHT lease registry, and Sphinx `invoke` pipeline without introducing WebRTC, STUN, ICE, or TURN primitives into `rings-core` or `rings-node`.

## Single-Hop Threat Model (Phase A)

- **Anonymous ticket issuance (`turn.ticket` over Onion `invoke`)**: any Rings node (native Rust `request_turn_ticket`, shipped `librings_node` C FFI / `rings-node` browser WASM via generic `"lookupRegistry"` + `"onionInvoke"`, or `rings-turn-service` WASM bindings) acquires a `SignedTurnSessionTicket` over a guard-closed Sphinx `invoke` loop (`invoke<64,1024,500ms>`, backend `"turn.ticket"`). The TURN node sees only the preceding relay hop and a single-use `ephemeral_client_pubkey`, learning neither the client's `Did` nor its IP address during reservation, and binds zero OS relay sockets until `Allocate`.
- **Standard TURN relay (`RFC 5766`)**: when `RTCPeerConnection` connects to `turn://tkt-…:<pass>@host:port`, the single-hop TURN server sees the client's source IP and the peer's IP (`XOR-PEER-ADDRESS`), **but never learns the client's Rings `Did`** (`tkt-` `USERNAME` carries zero `Did` or `Delegation` bytes).
