# rings-turn-service

External overlay glue connecting `rings-turn` (`RFC 5766` TURN server and unlinkable `SessionTicket` credentials) to `rings-node`'s generic foreground daemon, DHT lease registry, Sphinx `invoke` pipeline, and generic `onion::udp` exit sessions without introducing WebRTC, STUN, ICE, or TURN primitives into `rings-core` or `rings-node`.

## Privacy Modes & Threat Model

1. **Anonymous ticket issuance (`turn.ticket` over Onion `invoke`)**: any Rings node (native Rust `request_turn_ticket`, shipped `librings_node` C FFI / `rings-node` browser WASM via generic `"lookupRegistry"` + `"onionInvoke"`, or `rings-turn-service` WASM bindings) acquires a `SignedTurnSessionTicket` over a guard-closed Sphinx `invoke` loop (`invoke<64,1024,500ms>`, backend `"turn.ticket"`). The TURN node sees only the preceding relay hop and a single-use `ephemeral_client_pubkey`, learning neither the client's `Did` nor its IP address during reservation, and binds zero OS relay sockets until `Allocate`.
2. **Single-hop private TURN (`multi_hop_onion_udp: false`, default)**: low-latency zero-DID profile. When `RTCPeerConnection` connects to `turn://tkt-…:<pass>@host:port`, the TURN server relays UDP locally. It observes the client's source IP and the peer's IP (`XOR-PEER-ADDRESS`), **while never learning the client's Rings `Did`**.
3. **Multi-hop split-trust TURN over `onion::udp` (`multi_hop_onion_udp: true`)**: Tor-grade endpoint-separation profile. The entry `TurnServer` bridges each allocation via `OnionUdpRelayFactory` to a dedicated 5-hop guard-closed `onion::udp` session (`Entry -> Guard -> Middle_1 -> Exit -> Middle_2 -> Guard -> Entry`):
   - **Entry (`rings-turn-service`, trusted for client↔entry linkage)**: sees `IP_client`, unlinkable `tkt-…`, `E_exit`, and `XOR-PEER-ADDRESS` control frames, **never** the client's `Did`, and never emits direct UDP packets to `IP_peer` from its own IP (`XOR-RELAYED-ADDRESS` is the exit's bound `E_exit`).
   - **Intermediate `relay` hops**: see only fixed-width AEZ-sealed Sphinx cells (`16 KiB` at `OnionLoopClass::DEFAULT`), learning neither `IP_client`, `IP_peer`, STUN transaction IDs, nor datagram lengths.
   - **Exit (`rings-node` offering generic `udp`)**: binds a plain OS `UdpSocket` (`E_exit`) with stateful peer-IP pinholes (`OnionUdpPinholeTable`) and per-session outbound datagram rate limiting (`MAX_OUTBOUND_DATAGRAMS_PER_SEC`), seeing only its immediate preceding/succeeding relay hops and `(E_exit, IP_peer)` — **never** `IP_client`, `Did`, or TURN credentials.
   - **Profile characteristics**: `Allocate` aligns to the exit's `250 ms` open-response quantum plus 5-hop Sphinx loop setup, active bursts pace across 3 forward + 3 return `send_outbox`-shaped overlay links (`~10.17 ms/slot` active rate at `16 KiB`, with zero idle cover overhead when inactive under `OnionIdleFloor::OFF`), and UDP datagrams multiplex over an ordered reliable `OnionSession` stream. Choose `false` for minimum interactive media latency, or `true` (sizing ICE/DTLS timeouts accordingly) when full entry/exit IP separation is desired.


## Installation

Install the standalone `rings-turn-service` binary from the workspace:

```sh
cargo install --path crates/turn-service
```

Or build it directly:

```sh
cargo build --release -p rings-turn-service --bin rings-turn-service
```

## Cargo Features

| Feature | Default | Description |
| --- | --- | --- |
| `native` | yes | Native daemon runtime (`rings-node/node`, `rings-turn/default`, `tokio`, `serde-saphyr`) |
| `cli` | yes | Standalone `rings-turn-service` CLI binary (implies `native`) |
| `browser` | no | Browser WebAssembly ticket and directory helpers (`wasm_lookup_turn_services`, `wasm_request_turn_ticket`) |
| `dummy` | no | In-memory deterministic simulation runtime (`rings-core/dummy`, `rings-node/dummy`, `rings-turn/default`) |

## CLI Usage (`rings-turn-service`)

Generate a configuration template (`rings-turn-service.yaml` by default):

```sh
rings-turn-service init --output rings-turn-service.yaml --delegatee-key ~/.rings/delegatee_key --public-ip 203.0.113.10
```

Start the TURN server and co-located `rings-node` daemon:

```sh
rings-turn-service run --config rings-turn-service.yaml
```

Optional overrides on `rings-turn-service run`:

- `--storage-path <PATH>` and `--storage-capacity <BYTES>`
- `--bootstrap-seed <FILE_OR_URL>`
- `--api-token-path <PATH>`, repeatable `--api-allowed-origin <ORIGIN>`, and `--allow-remote-external-api`

## Configuration Overview (`TurnServiceDaemonConfig`)

`rings-turn-service` reads a `TurnServiceDaemonConfig` YAML document with two top-level keys:

- `node`: standard `rings-node` configuration (`advertise_onion_relay: true` is required when `turn_service.issue_tickets: true`).
- `turn_service`: `TurnServiceConfig` settings covering `listen_addr`, `public_ip`, `advertised_ip`, `turn_url`, `relay_bind_ip`, `min_port`, `max_port`, `realm` (default `rings`), `server_key`, `allow_private_peers`, `max_nonces`, `max_allocations`, `max_reservations`, `max_reservation_ttl_secs` (`60`), `max_permissions_per_allocation`, `channel_bind_timeout_secs`, `allow_dids` (omit for any valid delegation/signature; `[]` for ticket-only mode; `[0x...]` for a delegator allowlist), `heartbeat_interval_secs` (`30`), `ttl_secs` (`90`), `issue_tickets` (`true`), `ticket_invoke_policy`, and `multi_hop_onion_udp` (`false`).

## Discovery & Client APIs

- **Service descriptors**: hosts publish signed `TurnServiceDescriptor` records to the `"turn_services"` DHT topic (`TURN_SERVICES_TOPIC`, signed under `TURN_SERVICE_DESCRIPTOR_DOMAIN_TAG`) alongside the `"turn.ticket"` onion `invoke` offer (`TURN_TICKET_INVOKE_SHAPE`: `invoke<64,1024,500ms>`).
- **Native / library clients**: query descriptors via `lookup_turn_services` / `lookup_turn_services_via_provider`, acquire unlinkable tickets over a Sphinx `invoke` loop via `request_turn_ticket` / `acquire_turn_ticket` / `acquire_turn_ticket_over_route`, or expose local helpers with `install_turn_client_rpc`.
- **Browser WebAssembly clients (`browser` feature)**: invoke `wasm_lookup_turn_services` and `wasm_request_turn_ticket`.
