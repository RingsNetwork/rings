# Upgrading to Rings 1.0

This document summarizes the contracts frozen in `1.0.0` and the changes required when upgrading
from `0.39.x` / `0.40.x` or `1.0.0-pre.*` builds.

## 1.0 Frozen Contracts

- **Wire framing (`WIRE_VERSION = 5`).** Every frame and handshake payload opens with the fixed
  header `"RINGS\0" ‖ wire_version` (`u16` big-endian). Nodes accept only their own `WIRE_VERSION`
  (`5` across the `1.0.x` line) and refuse any other version before body decode with
  `Error::IncompatibleWireVersion` (JSON-RPC error `210`). There is no multi-version negotiation:
  any future wire-version bump is a coordinated cutover, and `1.0.x` patch/minor releases preserve
  `WIRE_VERSION = 5`. Binary `postcard` serialization of `Did` (`[u8; 20]`) and `PublicKey<SIZE>`
  (`[u8; SIZE]`) and generalized multi-step binary handshake frames (`ConnectNodeSend` /
  `ConnectNodeReport` carrying `(step_index, final_step, session_id, Bytes)`) are cut over without
  legacy disk-format or text-SDP shims: under the `RecordAuthority` decode law, non-authoritative
  local stores (`Disposable`, `DurableCache`, and `Retained`, including `dht-removals` and
  `relay-inbox`) intentionally retire undecodable records from pre-`WIRE_VERSION = 5` schemas on
  read. `WIRE_VERSION = 5` also adds the world-facing onion service symbol `udp` and its
  length-prefixed session codec (`OnionUdpClientFrame` / `OnionUdpServerFrame`), so `udp` exit
  registrations and `onion::udp` sessions interoperate only between `WIRE_VERSION = 5` nodes.
- **Delegation verification and key binding.**
  - `DelegateeKey` deserialization and `FromStr` verify that `delegatee_secret_key` derives
    `delegation.delegatee_did()` and that the delegator signature over `delegation` is valid.
    Mismatched `(Delegation, SecretKey)` pairs are rejected with `Error::DelegateeKeyMismatch`.
  - `Delegation` verification enforces both ends of the validity window
    `[ts_ms - 3000 ms, ts_ms + ttl_ms]` under both initial (`Verify`) and cached (`Established`)
    authorization checks, rejecting post-dated delegations with `Error::DelegationNotYetValid`.
- **C FFI ABI (`crates/node/include/rings.h`).**
  `rings_node_new_provider_with_callback` takes an explicit capacity/written-length signer callback:
  `size_t (*signer)(const char *proof, char *out, size_t out_cap)` (`out_cap = 96`). The callback
  writes the exact signature length for `account_type` (`65` for `eip191`/`secp256k1`/`bip137`,
  `64` for `ed25519`/`secp256r1`, `96` for `bls12-381`) into `out` and returns the byte count
  written (`0` on failure).
- **JSON-RPC client (`rings-rpc`).**
  `rings_rpc::jsonrpc::Client::connect_with_did` returns `Result<ConnectWithDidResponse>` (matching
  `Method::ConnectWithDid`), rather than `Result<ConnectWithSeedResponse>`.
- **Control API and extension protocols.**
  - The legacy internal `/ws` WebSocket route is removed; applications register pure
    `Protocol::step` state machines paired with namespace-scoped `Interpret` shells on `Provider`.
  - Authenticated HTTP/JSON-RPC routes require `Authorization: Bearer <token>` (`api-token` next to
    `config.yaml`), and browser callers must be allowlisted via `api_allowed_origins` /
    `--api-allowed-origin`.

## Operator Upgrade Checklist (0.39 / 0.40 → 1.0)

1. **Upgrade all overlay peers to `WIRE_VERSION = 5`.** Verify a remote seed's wire version via
   `nodeDid` (`"wire_version": 5`) before connecting; pre-`1.0` nodes on earlier wire versions are
   refused with JSON-RPC error `210`. Because `WIRE_VERSION = 5` cuts over binary `Did` and
   `PublicKey` encodings and multi-step binary handshake frames without legacy decoders,
   pre-`WIRE_VERSION = 5` records in `Disposable`, `DurableCache`, and `Retained` stores (`data`,
   `dht-removals`, `relay-inbox`, `onion-entry-guards`, `measure`, `provisional-evidence`) are
   retired automatically on read under `RecordAuthority::retires_undecodable()`.
2. **Verify `config.yaml` fields.**
   - Remove deprecated keys (`ecdsa_key`, `session_manager`, `onion_http_proxy_service`,
     `max_circuits`, `max_streams_per_circuit`); `config.yaml` rejects unknown fields.
   - Ensure the eight required top-level fields (`network_id`, `delegatee_key`, `internal_api_port`,
     `external_api_addr`, `endpoint_url`, `stabilize_interval`, `data_storage`, `measure_storage`)
     are present. `ice_servers` is optional (defaulting to empty) and mutually exclusive with a
     tagged `network:` section; malformed `ice_servers` or `turn://...?pubkey=` entries now fail
     closed at startup instead of logging a warning and starting without traversal servers.
   - Storage CLI overrides map to `data_storage.path` (`~/.rings/data`) and
     `data_storage.capacity` (`200000000`).
3. **Choose a delegation rotation mode.**
   - *Online in-process renewal:* set `delegator_key: /path/to/ecdsa_key` (`0600`) in `config.yaml`
     so `rings run` renews `delegatee_key` automatically before the signing horizon (`expires_at_ms - 25 s`)
     without restarting, and monitor `session_key.rotation_succeeded_total` /
     `rotation_failed_total` via `GET /status?view=observability`.
   - *Offline issuance:* keep `delegator_key` unset, mint a fresh `delegatee_key` with
     `rings new-delegation` before the horizon, and restart `rings run`.
4. **Update C FFI and Rust RPC callers.**
   - Update C/Python/Swift/Kotlin FFI signer callbacks to accept `(proof, out, out_cap)` and return
     the written signature byte count.
   - Update Rust callers binding the return type of `rings_rpc::jsonrpc::Client::connect_with_did`
     to `ConnectWithDidResponse`.
5. **Published Cargo crates.**
   All 15 workspace crates (`rings-derive`, `rings-codec`, `rings-measure`, `rings-runtime`,
   `rings-aez`, `rings-network-policy`, `rings-dns-relay`, `rings-gateway`, `rings-webview`,
   `rings-transport`, `rings-core`, `rings-turn`, `rings-rpc`, `rings-node`, `rings-tui`) publish
   in topological dependency order at matching workspace versions.
