# Transport Abstraction & Custom Transports

`rings-core` has zero hard-coded dependency on WebRTC, TURN, or OS sockets. Instead, peer
connectivity lives in `crates/transport` behind object-safe async traits, decoupled configuration
descriptors, and runtime contracts provided by `crates/runtime`.

## Decoupled trait boundary (`rings-transport`)

`rings-core` interacts with network links solely through `crates/transport/src/core/transport/interface.rs`:

- **`TransportInterface` / `DynTransportInterface` / `SharedTransport`**: creates outbound
  connections, accepts inbound handshake steps, and registers the swarm's
  `InnerTransportCallback` without exposing ICE, SDP, or WebRTC types to the DHT or replay state
  machines.
- **`ConnectionInterface` / `DynConnection`**: owns connection lifecycle, `max_message_size()`
  (must satisfy `MIN_MAX_MESSAGE_SIZE = 6145` bytes so widest unchunked control payloads fit
  whole), four ordered credit-bounded [`ChannelLane`](transport-flow-control.md) channels, and the
  multi-step handshake driver.
- **`NodeNetworkParameters` & `NetworkConfigDescriptor`**: `crates/node/src/processor/config.rs`
  bridges a tagged YAML/JSON descriptor:

  ```yaml
  network:
    kind: webrtc
    params:
      ice_servers: "stun://stun.l.google.com:19302"
      external_ip: null
      udp_port_range: [40000, 40100]
  ```

  with `webrtc` registered by default (derived automatically from top-level `ice_servers`,
  `external_ip`, and `udp_port_min`/`udp_port_max` when `network` is omitted). External embedders
  can register alternative transport implementations (for example QUIC, BLE, or packet radio) via
  `NetworkConfigDescriptor` without patching `rings-core`.

## Generalized `0`–`8` step binary handshakes

`ConnectionInterface::handshake_step` replaces WebRTC-specific SDP strings inside `rings-core` with
opaque byte payloads (`DEFAULT_MAX_HANDSHAKE_STEP_BYTES = 4096`, `MAX_HANDSHAKE_STEPS = 8`) driven
by the three-variant `HandshakeStep` state machine:

| Pattern | Initiator emissions | Responder emissions | Example transport |
|---|---|---|---|
| **0-step** (pre-provisioned) | `Done` | `Done` | Pre-shared / connectionless link |
| **1-step** (unidirectional announce) | `SendFinal(m1)` | `Done` | Single-message announce |
| **2-step** (offer / answer) | `Send(offer)` → `Done` | `SendFinal(answer)` | WebRTC SDP offer/answer |
| **3-step** (mutual challenge) | `Send(m1)` → `SendFinal(m3)` | `Send(m2)` → `Done` | Noise XX or authenticated key exchange |

Each step carries `(step_index, final_step, session_id)` inside the overlay handshake frame so
concurrent crossing offers tie-break cleanly and session keys can rotate two-phase across transport
rebuilds.

## Built-in transport backends

1. **Native WebRTC (`native-webrtc`, `webrtc = "0.17.3"`)**:
   - Supports STUN and UDP/TCP TURN servers (`turn://`, `turns://`, `?transport=tcp`).
   - Includes a built-in TURN-over-TCP/TLS bridge (`crates/transport/src/connections/native_webrtc/turn_tcp.rs`) using
     `tokio-rustls` + `webpki-roots` to bridge WebRTC ICE over TCP and TLS (`3478` / `5349`)
     whenever OS UDP is blocked or `?transport=tcp` is requested.
   - Supports bounded `IceCandidateTier` settling and container-bridge filtering so candidate
     gathering does not stall on phantom virtual interfaces.
2. **Browser WebRTC (`web-sys-webrtc`)**:
   - Drives browser `RTCPeerConnection` and `RTCDataChannel` with quiet-window ICE settling so
     strict NAT/mobile hosts settle promptly without waiting for a full timeout.
3. **In-memory test transport (`dummy`)**:
   - Deterministic, socket-free simulation transport gated out of production `rings` binaries via
     compile-time checks, used by multi-node convergence and sync-storm test suites.

## Runtime isolation (`rings-runtime`)

`crates/runtime` isolates task spawning (`Spawner`), join handles (`TaskSet`), timers (`sleep`),
and `MaybeSendSync` bounds across Tokio multi-thread/current-thread executors on native hosts and
single-threaded WebAssembly (`wasm-bindgen-futures`) in browsers.

## Related chapters

- [Transport Flow Control](transport-flow-control.md) — 4-lane credit windows, `NodeFramePool`, and liveness probes
- [How handshake works](handshake.md) — end-to-end handshake paths and glare tie-breaking
- [TURN Server & Onion TURN](../turn-server.md) — stateless UDP/TCP TURN and Onion TURN integration
