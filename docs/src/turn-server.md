# TURN Server (`rings-turn`)

`rings-turn` is a standalone, stateless RFC 5766 TURN server for Rings Network that authenticates WebRTC relay allocations using Rings cryptographic signatures (**"signature as password"**) instead of a username/password database or shared HMAC secret.

{{#include ../../crates/turn/README.md:model}}

## Running `rings-turn`

Build or install the `rings-turn` binary from the workspace:

```bash
cargo build --release -p rings-turn --bin rings-turn
# or
cargo install --path crates/turn
```

### Command-line interface

```text
Usage: rings-turn [OPTIONS]

Options:
      --listen <LISTEN>
          UDP socket address to listen on for STUN/TURN requests
          [env: RINGS_TURN_LISTEN=] [default: 0.0.0.0:3478]
      --realm <REALM>
          TURN authentication realm advertised in 401/438 challenges
          [env: RINGS_TURN_REALM=] [default: rings.network]
      --public-ip <PUBLIC_IP>
          Public IPv4/IPv6 address advertised in XOR-RELAYED-ADDRESS.
          Defaults to the listener IP (or 127.0.0.1 when listening on 0.0.0.0)
          [env: RINGS_TURN_PUBLIC_IP=]
      --relay-bind-ip <RELAY_BIND_IP>
          Local IPv4/IPv6 address bound when allocating relay UDP sockets.
          Defaults to the listener IP
          [env: RINGS_TURN_RELAY_BIND_IP=]
      --min-port <MIN_PORT>
          Inclusive lower bound for allocated relay UDP ports
          [env: RINGS_TURN_MIN_PORT=]
      --max-port <MAX_PORT>
          Inclusive upper bound for allocated relay UDP ports
          [env: RINGS_TURN_MAX_PORT=]
      --channel-bind-timeout-secs <CHANNEL_BIND_TIMEOUT_SECS>
          ChannelBind timeout in seconds (0 uses the RFC 5766 default of 600s)
          [env: RINGS_TURN_CHANNEL_BIND_TIMEOUT_SECS=] [default: 0]
      --server-key <SERVER_KEY>
          Hex-encoded 32-byte secp256k1 secret key for TURN ECDH credential derivation.
          When omitted, an ephemeral key is generated on startup and its public key is logged
          [env: RINGS_TURN_SERVER_KEY=]
      --allow-did <ALLOW_DIDS>
          Optional comma-separated allowlist of DIDs allowed to allocate TURN relays.
          When omitted, any cryptographically valid Rings signature is admitted
          [env: RINGS_TURN_ALLOW_DIDS=]
      --allow-private-peers
          Allow relay sockets to exchange UDP packets with loopback, link-local, or private
          peer addresses (disabled by default to prevent SSRF / local-network amplification)
          [env: RINGS_TURN_ALLOW_PRIVATE_PEERS=]
      --max-nonces <MAX_NONCES>
          Maximum number of active nonce challenges tracked concurrently
          [env: RINGS_TURN_MAX_NONCES=] [default: 4096]
      --max-allocations <MAX_ALLOCATIONS>
          Maximum number of active TURN relay allocations stored concurrently
          [env: RINGS_TURN_MAX_ALLOCATIONS=] [default: 1024]
      --max-reservations <MAX_RESERVATIONS>
          Maximum number of unclaimed session-ticket reservations stored concurrently (0 disables)
          [env: RINGS_TURN_MAX_RESERVATIONS=] [default: 256]
      --max-reservation-ttl-secs <MAX_RESERVATION_TTL_SECS>
          Maximum lifetime in seconds of an unclaimed session-ticket reservation
          [env: RINGS_TURN_MAX_RESERVATION_TTL_SECS=] [default: 60]
  -h, --help
          Print help
  -V, --version
          Print version
```

### Deployment examples

1. **Public relay with bounded UDP port range** (open UDP `3478` and `49152-65535` in your firewall):

   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --relay-bind-ip 0.0.0.0 \
     --min-port 49152 \
     --max-port 65535 \
     --server-key 0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318
   ```

2. **Private relay restricted to an allowlist of DIDs**:

   ```bash
   rings-turn \
     --listen 0.0.0.0:3478 \
     --public-ip 203.0.113.10 \
     --server-key 0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318 \
     --allow-did 0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222
   ```

## Configuring nodes to use `rings-turn`

In a node's [`config.yaml`](advanced-topic/config.yaml.md) (or via `rings run --ice-servers`), list the `turn://` server with its `?pubkey=<base58(server_pubkey)>` query parameter (logged by `rings-turn` on startup) and without credentials:

```yaml
ice_servers: stun://stun.l.google.com:19302;turn://203.0.113.10:3478?pubkey=249495738f7s7d8f7s8d7f8s7d8f7s8d7f8s7d8f7s8d
```

- The node signs `turn://...?pubkey=<server_pubkey>` (and RFC 7065 `turn:...?pubkey=<server_pubkey>`) entries dynamically with its active `DelegateeKey` and ECDH shared secret, and automatically rotates TURN credentials in lockstep whenever `DelegateeKey` is renewed in process (for example when `delegator_key` is configured in `config.yaml`).
- Non-expiring account-level DID credentials are available programmatically for embedded/SDK callers via `WebrtcNetworkParameters::with_credential_mode(TurnCredentialMode::Account(...))`.
- Explicit `turn://user:pass@host:port` entries are left untouched for third-party TURN servers.

## DHT Service Discovery and Anonymous `turn.ticket` Issuance (`rings-turn-service`)

Neither `rings-core` nor `rings-node` embeds TURN or WebRTC service primitives. Instead, the external `rings-turn-service` crate (`rings-turn-service init` / `rings-turn-service run --config ...`) runs a full foreground Rings node daemon (persistent `NodeStores` including authoritative replay storage, `BootstrapSupervisor`, HTTP JSON-RPC endpoints, and delegation horizon renewal) via `rings_node::native::daemon::run_foreground_node`, composing `rings-turn` over `rings-node`'s generic DHT lease registry and Sphinx `invoke` pipeline. It publishes a signed `TurnServiceDescriptor` (`(did, delegatee_public_key, process_epoch, server_pubkey, turn_url)`) onto the `turn_services` DHT topic and issues unlinkable `SessionTicket` credentials over the `turn.ticket` onion `invoke` backend (`invoke<64,1024,500ms>`), holding zero OS relay sockets until the client authenticates `Allocate`:

```yaml
node:
  advertise_onion_relay: true
turn_service:
  listen_addr: 0.0.0.0:3478
  public_ip: 1.1.1.1
  turn_url: turn://turn.example.org:3478
  min_port: 49152
  max_port: 65535
  max_allocations: 1024
  max_reservations: 256
  max_reservation_ttl_secs: 60
  issue_tickets: true
  ticket_invoke_policy:
    max_concurrent: 16
    max_invocations_per_minute: 120
```

Any client node (native Rust `rings_turn_service::request_turn_ticket`, shipped `librings_node` C FFI / `rings-node` browser WASM via `rings-node`'s generic `"lookupRegistry"` + `"onionInvoke"` RPC primitives, or `rings-turn-service` WASM `lookupTurnServices` / `requestTurnTicket`) discovers live `TurnServiceDescriptor` entries from `turn_services`, sends `(ephemeral_client_pubkey, ttl_ms)` through a guard-closed Sphinx `invoke` loop matched to the exact `(did, process_epoch, delegatee_public_key)` stage, verifies the returned `SignedTurnSessionTicket` against the descriptor's `server_pubkey`, and receives a ready `turn://tkt-…:<pass>@host:port` ICE URL.

In this single-hop private TURN mode (`relay_mode: local`, the default), the TURN server necessarily observes the client's IP and remote peer's IP when relaying media, **but never learns the client's `Did` or overlay identity**.

## Multi-Hop Private TURN over Generic `onion::udp` (`relay_mode: onion_udp`)

Setting `turn_service.relay_mode: onion_udp` decouples the client-facing RFC 5766 TURN entry (`rings-turn-service`) from the world-facing UDP relay socket (`E_exit`) using `rings-node`'s generic `udp` exit symbol (`Σ_W`, zero TURN/WebRTC primitives in `rings-core` or `rings-node`):

1. **TURN Entry (`rings-turn-service`)**: runs standard RFC 5766 `Allocate`, `CreatePermission`, `ChannelBind`, and `Refresh` via `OnionUdpRelayBackend`:
   ```yaml
   node:
     advertise_onion_relay: true
   turn_service:
     listen_addr: 0.0.0.0:3478
     public_ip: 198.51.100.10
     turn_url: turn://turn-entry.example.org:3478
     relay_mode: onion_udp
     onion_loop_class: KiB16
     issue_tickets: true
   ```
2. **Middle Hops (`rings-node` with `advertise_onion_relay: true`)**: standard stateless Sphinx relay hops carrying fixed-width `16 KiB` cells (`OnionLoopClass::KiB16`).
3. **Generic `udp` Exit (`rings-node` with `onion_exit_services: [udp]`)**: binds a plain public OS UDP socket `E_exit`, enforces `rings_network_policy::is_public_ip` egress and stateful IP pinholes (so unsolicited internet scan traffic consumes zero return-loop SURBs and zero `OnionExitAccounting` quota), and returns `E_exit` in the session `Bound` frame, which the TURN entry projects as `XOR-RELAYED-ADDRESS`:
   ```yaml
   advertise_presence: true
   advertise_onion_relay: true
   advertise_onion_exit: true
   onion_exit_services:
   - udp
   onion_exit_policy:
     allowed_targets:
     - "*:*"
     denied_targets: []
     max_sessions: 512
     max_bytes_per_minute: 1073741824
   onion_udp_exit:
     bind_ipv4: 0.0.0.0
     advertised_ipv4: 203.0.113.50
     min_port: 49152
     max_port: 65535
   ```

### Multi-Hop Threat Model

| Party | Learns | Does NOT Learn |
| --- | --- | --- |
| TURN entry (`rings-turn-service`, `SessionTicket` + `onion_udp`) | Client IP, peer IP (`CreatePermission` / `Send` / `ChannelBind`), `E_exit`, relayed DTLS/SRTP ciphertext | Client `Did`, client `Delegation`, exit operator's overlay path (sealed inside Sphinx return/forward loops) |
| Middle relay hops (`OnionRole::Relay`) | Preceding hop and next hop on a `16 KiB` fixed-width cell loop | Client IP, client `Did`, peer IP/port, `E_exit`, STUN `TransactionId` / magic cookie, true datagram length |
| Generic `onion::udp` exit (`OnionRole::Exit([udp])`) | Peer IP/port, `E_exit`, raw UDP payload (DTLS/SRTP ciphertext) | Client IP, client `Did`, TURN `USERNAME` / `SessionTicket`, TURN entry IP |
| Remote WebRTC peer (relay-only policy) | Exit socket address `E_exit` (`XOR-RELAYED-ADDRESS`) | Client IP, client `Did`, TURN entry IP |

### Ordered Session Semantics & Measured Latency / Head-of-Line (HOL) Profile

Because `onion::udp` rides an ordered, reliable Sphinx session stream (`OnionLoopClass::KiB16` cells over constant-rate link pacing), relayed media experiences TURN-over-TCP-style ordered delivery rather than unordered UDP drop semantics. Reference measurements from `test_multihop_e2e::measure_single_hop_vs_multihop_turn_rtt_and_hol_burst` (`1 KiB` echo datagrams over real WebRTC overlay links + 4-relay/1-exit Sphinx loop on Apple Silicon loopback):

| Mode | `1 KiB` Echo RTT (`p50`) | `1 KiB` Echo RTT (`p95`) | `16 × 1 KiB` Back-to-Back Burst Completion |
| --- | --- | --- | --- |
| Single-hop (`relay_mode: local`) | `0.32 ms` (`324 µs`) | `0.45 ms` (`446 µs`) | `1.35 ms` (`1,348 µs`) |
| Multi-hop (`relay_mode: onion_udp`, `16 KiB` Sphinx cells) | `877.2 ms` (`877,184 µs`) | `942.9 ms` (`942,885 µs`) | `1,719.4 ms` (`1,719,410 µs`, coalesced across ordered session frames) |

