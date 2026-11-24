# Wire Protocol & LZ4 Compression

Rings frames every overlay message, link-control exchange, and chunked envelope behind a strict,
version-pinned binary wire contract implemented across `crates/codec` and
`crates/core/src/message/payload/`.

## Binary codec (`rings-codec`)

All structured wire types (`MessagePayload`, `LinkControl`, `Transaction`, `WireHandshake`,
registry descriptors, and storage payloads) encode with `rings-codec` (Postcard):

- Canonical serialization in compact variable-length binary form.
- Strict trailing-byte rejection on decode (`from_bytes` fails if any unconsumed byte remains).
- Compact binary `Did` (fixed 20 bytes) and secp256k1 `PublicKey` encodings rather than text hex or
  base58 strings inside wire frames.

## Frame header & version cutover (`WIRE_VERSION`)

Every frame transmitted over a Rings connection begins with an exact 9-byte header:

```text
frame        = MAGIC ‖ wire_version ‖ kind ‖ body
MAGIC        = "RINGS\0"          (6 bytes, fixed forever)
wire_version = u16 big-endian     (WIRE_VERSION = 6 in Rings 1.0)
kind         = u8                 (0 = Payload, 1 = LinkControl, 2 = CompressedPayload)
```

| `kind` byte | Variant | Meaning |
|---|---|---|
| `0` | `FrameKind::Payload` | Uncompressed postcard-encoded `WirePayload` (or self-contained `MessagePayload`) |
| `1` | `FrameKind::LinkControl` | Unsigned link-local delegation lease control (`Known`, `Unknown`) |
| `2` | `FrameKind::CompressedPayload` | LZ4-compressed `WirePayload` (`DhtControl` / `Storage` only) |

### Compact `WirePayload` encoding

`WirePayload` elides fields that are redundant on initial or direct hops:

- **`WireHopVerification`**: elides the duplicate delegation slot and identical `(ttl_ms, ts_ms)` stamps whenever the hop proof shares the origin's `DelegationRef` (`SameDelegationAndStamps` on every locally originated initial-hop frame, `SameDelegation` when only stamps differ, or `Distinct` on multi-hop relays).
- **`WireRelay`**: elides `destination` when it equals `transaction.destination` (`redirected_destination: None`, saving 20 bytes) and elides `next_hop` when it equals `relay.destination` (`intermediate_next_hop: None`, saving another 20 bytes on direct or final-hop frames).

### Version-cutover contract

- **Zero multi-version negotiation**: a node admits a frame or out-of-band handshake payload **only**
  when `wire_version == WIRE_VERSION` (`6`).
- **Pre-versioning rejection**: legacy pre-versioning frames opening with `RINGS-PAYLOAD` or
  `RINGS-LINK` (which differ at byte 6: `'-'` instead of `'\0'`) are explicitly rejected before
  deserialization as `IncompatibleWireVersion { theirs: BeforeVersioning }` (JSON-RPC error `210`).
- **Golden-vector enforcement**: `test_wire_goldens` across `rings-core`, `rings-node`, and
  `rings-turn-service` pins exact SHA-256 hashes of serialized wire layouts so any unintentional
  change breaks CI immediately.

## Bounded LZ4 compression (`CompressedPayload`)

To cut DHT control and storage replication bandwidth without exposing secret or fixed-width
traffic to compression-oracle or length-leak risks:

1. **Strict category boundary (`permits_wire_compression`)**: ONLY `MessageCategory::DhtControl`
   and `MessageCategory::Storage` frames may emit `FrameKind::CompressedPayload`.
   `MessageCategory::E2e` and `MessageCategory::Application` (including fixed-width Sphinx/AEZ
   onion cells) **never compress on the wire**.
2. **Static protocol dictionary (`WIRE_COMPRESSION_DICT`)**: `lz4_flex::block::compress_with_dict`
   uses an in-crate compile-time dictionary (`<= 64 KiB`) seeded with WebRTC SDP/ICE attributes,
   delegation tags, and URI boilerplate. Compression is emitted **only** when the compressed body
   is strictly smaller than the uncompressed body; otherwise the frame uses `FrameKind::Payload`.
3. **Decompression-bomb defense (`MAX_COMPRESSED_PAYLOAD_BYTES`)**: both compression and
   decompression enforce an explicit ceiling equal to `MAX_CHANNEL_MESSAGE_SIZE` (`64 KiB` /
   `65,536` bytes) for the uncompressed `WirePayload`, allocating at most that ceiling before
   reading untrusted LZ4 tokens and charging inbound memory and reassembly quotas against the
   **uncompressed** frame length (`from_wire_with_uncompressed_len`).

## Large-message chunking

Logical messages larger than a connection's negotiated `max_message_size` (up to
`TRANSPORT_MAX_SIZE = 60_000_000` bytes / 60 MB) are segmented into `MessagePayload` chunks,
reassembled by bounded `ReassemblyLimits` (`production` or `constrained` profile), and charged to
inbound lane credit and origin quotas once per logical message.

## Related chapters

- [Delegation References on a Link](delegation-references.md) — inline vs 20-byte digest references inside `WirePayload`
- [Transaction Replay Protection](transaction-replay.md) — signing domains and per-class replay windows
- [API](../jsonrpc.md#wire-version) — inspecting `wire_version` via `nodeDid` and error `210`
