# rings-turn-service

External overlay glue connecting `rings-turn` (`RFC 5766` TURN server and unlinkable `SessionTicket` credentials) to `rings-node`'s generic foreground daemon, DHT lease registry, Sphinx `invoke` pipeline, and generic `onion::udp` exit sessions without introducing WebRTC, STUN, ICE, or TURN primitives into `rings-core` or `rings-node`.

## Privacy Modes & Threat Model

1. **Anonymous ticket issuance (`turn.ticket` over Onion `invoke`)**: any Rings node (native Rust `request_turn_ticket`, shipped `librings_node` C FFI / `rings-node` browser WASM via generic `"lookupRegistry"` + `"onionInvoke"`, or `rings-turn-service` WASM bindings) acquires a `SignedTurnSessionTicket` over a guard-closed Sphinx `invoke` loop (`invoke<64,1024,500ms>`, backend `"turn.ticket"`). The TURN node sees only the preceding relay hop and a single-use `ephemeral_client_pubkey`, learning neither the client's `Did` nor its IP address during reservation, and binds zero OS relay sockets until `Allocate`.
2. **Single-hop private TURN (`multi_hop_onion_udp: false`, default)**: when `RTCPeerConnection` connects to `turn://tkt-…:<pass>@host:port`, the TURN server relays UDP locally. It observes the client's source IP and the peer's IP (`XOR-PEER-ADDRESS`), **but never learns the client's Rings `Did`**.
3. **Multi-hop private TURN over `onion::udp` (`multi_hop_onion_udp: true`)**: the entry `TurnServer` bridges each allocation via `OnionUdpRelayFactory` to a dedicated 5-hop guard-closed `onion::udp` session (`Entry -> Guard -> Middle_1 -> Exit -> Middle_2 -> Guard -> Entry`):
   - **Entry (`rings-turn-service`)**: sees `IP_client`, unlinkable `tkt-…`, and `XOR-PEER-ADDRESS` control frames, **never** the client's `Did`, and never emits direct UDP packets to `IP_peer` from its own IP (`XOR-RELAYED-ADDRESS` is the exit's bound `E_exit`).
   - **Intermediate `relay` hops**: see only fixed-width AEZ-sealed Sphinx cells (`16 KiB` at `OnionLoopClass::DEFAULT`), learning neither `IP_client`, `IP_peer`, STUN transaction IDs, nor datagram lengths.
   - **Exit (`rings-node` offering generic `udp`)**: binds a plain OS `UdpSocket` (`E_exit`) with stateful peer-IP pinholes (`OnionUdpPinholeTable`), seeing only its immediate preceding/succeeding relay hops and `(E_exit, IP_peer)` — **never** `IP_client`, `Did`, or TURN credentials.
