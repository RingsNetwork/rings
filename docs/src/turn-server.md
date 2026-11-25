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
          UDP and TCP socket address to listen on for STUN/TURN requests (binds both UDP and TCP; up to 512 TCP connections)
          [env: RINGS_TURN_LISTEN=] [default: 0.0.0.0:3478]
      --realm <REALM>
          TURN authentication realm advertised in 401/438 challenges
          [env: RINGS_TURN_REALM=] [default: rings]
      --server-key <SERVER_KEY>
          Hex-encoded 32-byte secp256k1 secret key for TURN ECDH credential derivation.
          When omitted, an ephemeral key is generated on startup and its public key is logged
          [env: RINGS_TURN_SERVER_KEY=]
      --public-ip <PUBLIC_IP>
          Public IPv4/IPv6 address advertised in XOR-RELAYED-ADDRESS.
          Defaults to --relay-bind-ip (or localhost of --relay-bind-ip's address family when unspecified)
          [env: RINGS_TURN_PUBLIC_IP=]
      --advertised-ip <ADVERTISED_IP>
          Optional public IPv4/IPv6 address advertised for the TURN listener URL.
          Defaults to --public-ip when its address family matches --listen, or the listener IP
          [env: RINGS_TURN_ADVERTISED_IP=]
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
      --max-permissions-per-allocation <MAX_PERMISSIONS_PER_ALLOCATION>
          Maximum number of active peer IP permissions per TURN allocation
          [env: RINGS_TURN_MAX_PERMISSIONS_PER_ALLOCATION=] [default: 64]
      --channel-bind-timeout-secs <CHANNEL_BIND_TIMEOUT_SECS>
          ChannelBind timeout in seconds (0 uses the RFC 5766 default of 600s)
          [env: RINGS_TURN_CHANNEL_BIND_TIMEOUT_SECS=] [default: 0]
      --allow-did <ALLOW_DIDS>
          Optional comma-separated allowlist of DIDs allowed to allocate TURN relays.
          When omitted, any cryptographically valid Rings signature is admitted
          [env: RINGS_TURN_ALLOW_DIDS=]
  -h, --help
          Print help
  -V, --version
          Print version
```

### Deployment examples

1. **Public relay with bounded UDP port range** (open UDP **and TCP** `3478` plus relay UDP `49152-65535` in your firewall):

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

## TURN over TCP (`?transport=tcp`) and `turns://`

`rings-turn` automatically binds **both UDP and TCP** listeners on `--listen` (capped at `512`
concurrent TCP client connections using RFC 5766 / RFC 6062 STUN/ChannelData framing).

- **TCP traversal (`?transport=tcp`)**: on UDP-blocked or strict corporate/mobile networks,
  configure `ice_servers` with `turn://203.0.113.10:3478?transport=tcp&pubkey=<server_pubkey>`.
  On native hosts, `rings-transport`'s built-in TURN-over-TCP/TLS bridge (`turn_tcp`) opens the TCP/TLS control and
  ChannelData link to the TURN server even when local OS UDP egress is blocked.
- **TLS termination (`turns://`)**: `rings-turn` does not terminate TLS inside the server binary.
  To serve `turns://turn.example.org:5349?pubkey=<server_pubkey>`, place a TLS terminator (such
  as Caddy, Nginx stream, or HAProxy) on TCP port `5349` forwarding plaintext TCP to `rings-turn`'s
  `--listen` port.

## Configuring nodes to use `rings-turn`

In a node's [`config.yaml`](advanced-topic/config.yaml.md) (or via `rings run --ice-servers`), list the `turn://` (or `turns://`, optionally with `?transport=tcp`) server with its `?pubkey=<base58(server_pubkey)>` query parameter (logged by `rings-turn` on startup) and without credentials:

```yaml
ice_servers: stun://stun.l.google.com:19302;turn://203.0.113.10:3478?transport=tcp&pubkey=249495738f7s7d8f7s8d7f8s7d8f7s8d7f8s7d8f7s8d
```

- The node signs `turn://...?pubkey=<server_pubkey>` (and `turns://` / RFC 7065 `turn:`/`turns:`) entries dynamically with its active `DelegateeKey` and ECDH shared secret, and automatically rotates TURN credentials in lockstep whenever `DelegateeKey` is renewed in process (for example when `delegator_key` is configured in `config.yaml`).
- Non-expiring account-level DID credentials are available programmatically for embedded/SDK callers via `WebrtcNetworkParameters::with_credential_mode(TurnCredentialMode::Account(...))`.
- Explicit `turn://user:pass@host:port` entries are left untouched for third-party TURN servers.

## DHT Service Discovery and Anonymous `turn.ticket` Issuance (`rings-turn-service`)

Neither `rings-core` nor `rings-node` embeds TURN or WebRTC service primitives. Instead, the external `rings-turn-service` crate (`rings-turn-service`) runs a full foreground Rings node daemon (persistent `NodeStores` including authoritative replay storage, `BootstrapSupervisor`, HTTP JSON-RPC endpoints, and delegation horizon renewal) via `rings_node::native::daemon::run_foreground_node`, composing `rings-turn` over `rings-node`'s generic DHT lease registry and Sphinx `invoke` pipeline. It publishes a signed `TurnServiceDescriptor` (`(did, delegatee_public_key, process_epoch, server_pubkey, turn_url)`) onto the `turn_services` DHT topic and issues unlinkable `SessionTicket` credentials over the `turn.ticket` onion `invoke` backend (`invoke<64,1024,500ms>`), holding zero OS relay sockets until the client authenticates `Allocate`.

### Install and generate configuration

```sh
cargo +1.97.0 install --locked rings-turn-service
rings-turn-service init --output rings-turn-service.yaml --public-ip 203.0.113.10
rings-turn-service run --config rings-turn-service.yaml
```

`rings-turn-service init` generates a complete `TurnServiceDaemonConfig` YAML containing both a
full `node:` section (`rings_node::native::config::Config`, with `advertise_onion_relay: true`)
and a `turn_service:` section (plus an owner-only `api-token` next to the file). Note that
`node:` requires the same mandatory keys as `config.yaml` (`network_id`, `delegatee_key`,
`internal_api_port`, `external_api_addr`, `endpoint_url`, `stabilize_interval`, `data_storage`,
`measure_storage`), and `issue_tickets: true` requires `node.advertise_onion_relay: true`:

```yaml
node:
  network_id: 1
  delegatee_key: ~/.rings/delegatee_key
  internal_api_port: 50000
  external_api_addr: 127.0.0.1:50001
  endpoint_url: http://127.0.0.1:50000
  stabilize_interval: 15
  advertise_onion_relay: true
  data_storage:
    path: ~/.rings/data
    capacity: 200000000
  measure_storage:
    path: ~/.rings/measure
    capacity: 200000000
turn_service:
  listen_addr: 0.0.0.0:3478
  public_ip: 203.0.113.10
  advertised_ip: 203.0.113.10
  turn_url: turn://turn.example.org:3478
  relay_bind_ip: 0.0.0.0
  min_port: 49152
  max_port: 65535
  realm: rings
  allow_private_peers: false
  max_nonces: 4096
  max_allocations: 1024
  max_reservations: 256
  max_reservation_ttl_secs: 60
  max_permissions_per_allocation: 64
  channel_bind_timeout_secs: 0
  heartbeat_interval_secs: 30
  ttl_secs: 90
  issue_tickets: true
  ticket_invoke_policy:
    max_concurrent: 16
    max_invocations_per_minute: 120
  multi_hop_onion_udp: false
```

`turn_service.allow_dids` is optional and defaults to omitted (`None`), which admits any valid live
delegation or account signature (`AllowAnyDelegation`). Explicitly setting `allow_dids: []`
configures **ticket-only mode** (rejecting non-ticket delegations/account credentials while still
admitting server-issued `turn.ticket` `SessionTicket` credentials), whereas setting
`allow_dids: ["0x..."]` restricts non-ticket authentication to the listed delegator DIDs while still
admitting server-issued session tickets.

`rings-turn-service run` also accepts `--storage-path`, `--storage-capacity`, `--bootstrap-seed`,
`--api-token-path`, `--api-allowed-origin`, and `--allow-remote-external-api`.

### Client discovery & ticket acquisition

Any client node can discover live `TurnServiceDescriptor` entries from `turn_services`, send `(ephemeral_client_pubkey, ttl_ms)` through a guard-closed Sphinx `invoke` loop matched to the exact `(did, process_epoch, delegatee_public_key)` stage, verify the returned `SignedTurnSessionTicket` against the descriptor's `server_pubkey`, and receive a ready `turn://tkt-…:<pass>@host:port` ICE URL:

- **Native Rust**: call `rings_turn_service::request_turn_ticket` (or install `"lookupTurnServices"` and `"requestTurnTicket"` local RPC handlers via `install_turn_client_rpc(provider)`).
- **C FFI / Browser WASM (`rings-node`)**: call local RPC [`lookupRegistry`](jsonrpc.md#provider-local-rpc-methods) with `{"topic": "turn_services"}` followed by [`onionInvoke`](jsonrpc.md#provider-local-rpc-methods) on backend `"turn.ticket"` at shape `"invoke<64,1024,500ms>"`.
- **Browser WASM (`rings-turn-service --features browser`)**: call the exported `lookupTurnServices` and `requestTurnTicket` helpers directly.

### Single-hop vs. multi-hop Onion TURN

- **Single-hop private TURN (`multi_hop_onion_udp: false`, default)**: the TURN server binds a local UDP socket for each allocation. It observes the client's IP and remote peer's IP when relaying media, **but never learns the client's `Did` or overlay identity**.
- **Multi-hop private TURN (`multi_hop_onion_udp: true`, experimental opt-in)**: `rings-turn-service` attaches `OnionUdpRelayFactory` to `TurnServer`, opening a dedicated 5-hop guard-closed [`onion::udp`](onion.md#world-facing-exits) session to a native exit node offering `udp` (`advertise_onion_exit: true`) for each `Allocate`:
  - `XOR-RELAYED-ADDRESS` returned to the unmodified WebRTC/TURN client is the exit's bound UDP socket (`E_exit`).
  - `CreatePermission` and `ChannelBind` enqueue stateful peer-IP pinholes (`OnionUdpPinholeTable`) onto the ordered `onion::udp` stream ahead of subsequent outbound datagrams, dropping unpermitted inbound peer traffic at the exit before consuming return SURBs.
  - Intermediate `relay` hops observe only fixed-width AEZ-sealed Sphinx cells, while the exit observes `(E_exit, IP_peer)` without learning `IP_client` or `Did` (note that the entry TURN node still observes `IP_client`, `E_exit`, and `XOR-PEER-ADDRESS`, never `Did`).
  - **Latency & head-of-line considerations**: socket opens pad to `250 ms` quanta, cells are paced by per-link constant-rate `send_outbox` slots across 6 overlay hops per round trip, and datagrams share an ordered reliable `OnionSession` stream (incurring HOL blocking on overlay packet loss). Leave `multi_hop_onion_udp: false` for standard low-latency deployments unless exit IP separation is explicitly needed.
