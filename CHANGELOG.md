# Changelog

## 0.21.0

### Breaking changes

- Message, transaction, and descriptor signatures now sign a domain-separated transcript that
  binds the signer's `network_id` and a per-message-family tag. Signatures issued by earlier
  builds no longer verify, and a signature issued inside one overlay does not verify inside
  another. `Transaction`, `MessagePayload`, and `PayloadSender` take a `MessageSigner` (a session
  key acting inside one overlay) instead of a bare `SessionSk`, and
  `MessageVerificationExt::verify` takes the receiver's `network_id`.
- DHT entries carry a retention bound (`expires_at_ms`) stamped at the operation boundary with
  `DEFAULT_TTL_MS` and bounded at admission by `MAX_TTL_MS`. Stored values without a bound, or
  whose bound has elapsed, are retired on their next read and are never replicated or served.
- Storage admission rejects peer-supplied CRDT versions whose logical time is more than
  `TS_OFFSET_TOLERANCE_MS` ahead of the receiver's clock, so a forged register floor can no longer
  pin a key.

### Fixed

- The native file-backed storage now enforces its configured byte capacity: a write beyond the
  budget retires the least recently written keys first, a value larger than the whole budget is
  rejected with `Error::StorageValueExceedsCapacity`, and the budget is restored on open.
- The local fetched-entry cache is bounded at `LOCAL_CACHE_CAPACITY` entries, evicting the least
  recently written entry.

## 0.20.2

### Added

- Bound the number of peers holding a logical connection record at twice the topology
  reference slots. A reservation against a full table evicts one admitted peer that no local
  topology slot references, preferring a generation already revoked by a send failure and then
  the peer silent longest among those older than the retention grace, and otherwise fails with
  `Error::ConnectionCapacityExceeded`.

### Fixed

- Chord `find_successor` now forwards to the successor head when no finger precedes the target
  instead of addressing the local node, so finger fixing and lookups over a sparse finger table
  make progress rather than failing on a self hop.

## 0.20.0

### Breaking changes

- Secret signing keys are no longer `Copy`; signing helpers now borrow keys and return explicit
  errors instead of silently substituting invalid signatures or scalars.
- Secret key containers now zeroize their long-lived scalar or seed storage on drop.
- Secp256r1 verification now rejects high-s signatures, so persisted high-s secp256r1 session
  proofs from older builds no longer verify. Ed25519 verification now uses strict signature
  validation.

### Added

- Add the opt-in native IPv4/TCP gateway for explicitly selected destination prefixes, with
  Linux TUN, macOS utun, Windows Wintun, and a separately launched Unix privilege helper.
- Add Swift and Kotlin/JNA desktop FFI examples aligned with the existing Python ownership and
  provider lifecycle.

## 0.19.0

### Breaking changes

- The unused Subring DHT model and `SubringInterface` are removed. This shifts the Postcard
  discriminants for `EntryKind::RelayMessage` and the operations after the former
  `EntryOperation::JoinSubring` slot. Mixed-version networking with nodes running `v0.17.x` or
  earlier is unsupported. Native and browser persistent DHT storage must be wiped before upgrading:
  old Subring entries can otherwise decode as relay messages, while old relay-message entries fail
  to decode from native Sled storage.
- The repository-maintained Nova proof application is removed, including `rings-snark`,
  `rings-snark-extension`, its examples, and the browser proof workbench. The last tagged Rings
  source release containing that application is `v0.17.0`; the last published `rings-snark` crate
  is `0.12.0`, while `rings-snark-extension` was not published on crates.io. Existing users can pin
  or fork those versions. Rings does not select a replacement proof system and does not yank the
  historical crate.
- `rings_transport::core::transport::TransportMessage::Custom` now stores `bytes::Bytes` instead
  of `Vec<u8>`.
  Migrate owned vectors with `TransportMessage::Custom(data.into())`. Its wire encoding remains
  compatible with 0.17.
- Native `rings_core::swarm::callback::SwarmCallback` methods now return
  `rings_core::swarm::callback::CallbackError`, whose error source must be `Send + Sync`.
  Implementations may also return `Box<dyn std::error::Error + Send + Sync>`; wasm callbacks keep
  the thread-local error bound.
- `rings_transport::core::callback::TransportCallback::on_message(&str, &[u8])` is replaced by
  `on_admitted_message(AdmittedInboundMessage<'_>)`. Callback implementations should read the
  connection identifier and payload through `cid()` and `payload()` (or consume the value with
  `into_parts()`); this admission token proves that envelope decoding and raw-frame capacity
  reservation already succeeded.
- Custom `rings_transport::core::transport::TransportInterface` implementations must own one
  stable `Arc<rings_transport::callback::InboundFrameCapacity>` and return it from
  `inbound_frame_capacity()`. Construct the accountant once with `InboundFrameCapacity::new()` and
  share that same allocation across every connection created by the transport.
- Custom `rings_transport::core::transport::ConnectionInterface` implementations must update
  `send_message_with_permit`: at the final cancellable backend boundary, call
  `permit.try_mark_irrevocable()` and stop unless it yields a proof. After the backend queue accepts
  the bytes, consume the proof with `mark_accepted()` before returning success. If work fails or is
  abandoned after claiming the proof but before acceptance, retire and close that connection
  generation before returning.
- `rings_transport::core::pool::MessageSenderPool` is removed. Send admission now requires
  connection-generation retirement state, so custom backends should implement
  `ConnectionInterface::send_message_with_permit` on the connection instead of on a channel pool.
- Construct `rings_transport::callback::InnerTransportCallback` with
  `for_transport(&transport, cid, callback, notifier)` instead of `new(cid, callback, notifier)`, so
  callback admission shares the transport's frame accountant. The connection identifier field is
  now private; read it with `cid()`.

### Changed

- Default CLI and SDK logging now emit only error-level events unless callers explicitly select a
  more verbose level; high-frequency transport diagnostics moved to debug or trace.

### Fixed

- Bound inbound and outbound scheduling, chunk reassembly, and data-channel delivery lifetimes.
- Preserve typed callback and native send task error sources.
- Keep connection admission, cancellation, and same-class transfer ordering coherent under races.
- Emit the observable `Connecting` state before connection establishment completes.
- Run native and wasm transport tests with production timeout profiles; dummy-native transport
  tests keep a reduced profile for bounded failure witnesses.
