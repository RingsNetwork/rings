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

In this single-hop private TURN mode, the TURN server necessarily observes the client's IP and remote peer's IP when relaying media, **but never learns the client's `Did` or overlay identity**.
