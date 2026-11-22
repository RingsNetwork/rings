# rings-turn-service

External overlay glue connecting `rings-turn` (`RFC 5766` TURN server and unlinkable `SessionTicket` credentials) to `rings-node`'s generic foreground daemon, DHT lease registry, Sphinx `invoke` pipeline, and generic `onion::udp` exit sessions without introducing WebRTC, STUN, ICE, or TURN primitives into `rings-core` or `rings-node`.

## Threat Model (Single-Hop vs Multi-Hop)

- **Anonymous ticket issuance (`turn.ticket` over Onion `invoke`)**: any Rings node (native Rust `request_turn_ticket`, shipped `librings_node` C FFI / `rings-node` browser WASM via generic `"lookupRegistry"` + `"onionInvoke"`, or `rings-turn-service` WASM bindings) acquires a `SignedTurnSessionTicket` over a guard-closed Sphinx `invoke` loop (`invoke<64,1024,500ms>`, backend `"turn.ticket"`). The TURN node sees only the preceding relay hop and a single-use `ephemeral_client_pubkey`, learning neither the client's `Did` nor its IP address during reservation, and binds zero OS relay sockets until `Allocate`.
- **Single-hop relay (`relay_mode: local`, default)**: when `RTCPeerConnection` connects to `turn://tkt-…:<pass>@host:port`, the TURN server binds a local OS UDP socket and sees the client's IP and remote peer's IP (`XOR-PEER-ADDRESS`), **but never learns the client's Rings `Did`**.
- **Multi-hop relay (`relay_mode: onion_udp`, Phase B)**: `OnionUdpRelayBackend` opens a client-sealed Sphinx `onion::udp` session across standard `OnionRole::Relay` middle hops to a remote `rings-node` exit advertising `udp`, returning `E_exit` as `XOR-RELAYED-ADDRESS`:
  - **Entry (`rings-turn-service`)**: sees client IP and peer IP (`CreatePermission` / `Send` / `ChannelBind`), never `Did`.
  - **Middle hops (`OnionRole::Relay`)**: carry fixed-width `16 KiB` Sphinx cells (`OnionLoopClass::DEFAULT`) and learn **neither client IP, client `Did`, peer IP/port, `E_exit`, STUN transaction metadata, nor datagram length**.
  - **Exit (`rings-node` `onion::udp`, TURN-free)**: binds `E_exit` and sees peer IP + UDP payload (DTLS/SRTP ciphertext in WebRTC), **never client IP or client `Did`**.
