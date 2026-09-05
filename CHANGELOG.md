# Changelog

## 0.21.0

### Breaking changes

- Message, transaction, and descriptor signatures now sign a domain-separated transcript that
  binds the signer's `network_id` and a per-message-family tag. Signatures issued by earlier
  builds no longer verify, and a signature issued inside one overlay does not verify inside
  another. Every signature is issued by a `MessageSigner` (a session key acting inside one
  overlay; `MessageSigner<&SessionSk>` borrowed, `MessageSigner<SessionSk>` owned) and verified
  under the receiver's overlay: `Transaction`, `MessagePayload`, `PayloadSender`, and the node
  descriptors take a `MessageSigner`, `MessageVerificationExt::verify` and the descriptor
  `verify_signature` / `is_live_at` / `latest_valid_by_*` take the receiver's `network_id`, and
  signing a descriptor whose body states another overlay is refused. Domain tags are built with
  `domain_tag!`.
- DHT entries carry a retention bound (`expires_at_ms`) stamped at the operation boundary with
  `DEFAULT_TTL_MS` and bounded at admission by `MAX_TTL_MS`. Stored values without a bound, or
  whose bound has elapsed, are retired on their next read and are never replicated or served.
  The native file store also retires a record the current schema cannot decode on the read that
  discovers it, so values written by earlier builds are dropped instead of failing every later
  write to their key.
- Storage admission rejects peer-supplied CRDT versions whose logical time is more than
  `TS_OFFSET_TOLERANCE_MS` ahead of the receiver's clock, so a forged register floor can no longer
  pin a key, and rejects any payload element larger than `ENTRY_PAYLOAD_MAX_BYTES` (32 KiB), so a
  carrier holds at most `ENTRY_DATA_MAX_LEN * ENTRY_PAYLOAD_MAX_BYTES` encoded bytes. Admission is
  applied to the peer-supplied delta, never to the receiver's join result.
- `Entry::operate`, `overwrite`, `extend`, `touch`, `compact_data`, and `EntryOperation::stamped`
  take the operation-boundary time explicitly; the clock is read only at the storage boundary.
  `Entry::extend` applies to relay inboxes as well as data topics. `Session::verify_at`,
  `MessageVerification::{verify_at, verify_live, verify_live_at}`, and
  `MessageVerificationExt::verify_at` judge a signature as of a given instant;
  `MessageVerification::verify_unexpired` is `verify_live`. A full carrier fits one transport
  message by a compile-time law over `ENTRY_DATA_MAX_LEN` and `ENTRY_PAYLOAD_MAX_BYTES`.
- `rings_core::storage::sled::SledStorage` is `rings_core::storage::file::FileStorage`, the name
  of what it has been since the byte budget landed: one file per key under a budget. The three
  poisoned-lock errors (`DHTSyncLockError`, `CallbackSyncLockError`, `StorageLockPoisoned`) are
  one `Error::LockPoisoned`.

### Added

- Relay inbox for offline peers. A `CustomMessage` that reaches the node responsible for its
  destination's ring position while the destination has no connection is *held*: wrapped in a
  `HeldMessage` under the holder's signature (domain
  `rings-core:relay-inbox:held-message:v1`, timestamp = hold instant) and written into the inbox
  carrier `destination + 1` (`EntryKind::RelayMessage`), which is placed by the ring geometry in
  every storage mode. Storage admits an inbox element only under a witness the owner verifies
  itself: it is a held `CustomMessage` addressed to the inbox's peer, held no later than the
  receiver's clock, whose holder signature verifies inside the local overlay and whose payload
  verifies *as of the hold instant* (`MessageVerificationExt::verify_at`; a sender's session
  expiring later does not unverify a held message, and a holder can hold only inside the sender's
  proof lifetime). Authority is checked at the write: a hold only from the node the owner routes
  the destination to and only while the message's sender proof is still live by the owner's own
  clock, a removal only from the recipient, a relocation only as an ownership hand-off from the
  authenticated predecessor; a relay carrier is never fetched, cached, replicated, or returned to
  a lookup. Removal is per element by its add dot (never a reset floor), the
  carrier keeps at most `RELAY_INBOX_MAX_LEN` (64) newest messages, and inboxes are retained for
  `DEFAULT_RELAY_INBOX_TTL_MS` (24 h) after the last hold, up to `MAX_RELAY_INBOX_TTL_MS` (7 d).
  When the peer returns its inbox key falls into its own storage interval and the predecessor's
  repair pass hands it over; every storage maintenance pass the peer drains its local inbox
  through the inbound pipeline (application validation, dispatch, `on_inbound`, each under the
  inbound deadline) and tombstones each element as it is delivered. Delivery is at least once.

### Fixed

- Storage ownership hand-off is now part of the storage repair pass: the pass drains the local
  inbox, offers the live local entries placed beyond `(self, successor head]` to the head (the
  receiver's acknowledgement removes the local copy), and republishes to missing affine owners,
  all through the repair delivery window with its fresh-connection grace. Every topology
  transition that moves the successor head (`TopologyAction::SuccessorHeadChanged`) requests a
  repair round instead of sending. Previously the hand-off ran only when the old successor
  replied with `NotifyPredecessorReport`, so a head moved by a topology query or by a directly
  connected peer left entries at a node that no longer owned them until they expired.
- The native file-backed storage now enforces its configured byte capacity: a write beyond the
  budget retires the least recently written keys first, a value larger than the whole budget is
  rejected with `Error::StorageValueExceedsCapacity`, the index is updated only after the file
  system operation succeeded, and the budget is restored on open.
- The local fetched-entry cache is bounded at `LOCAL_CACHE_CAPACITY` entries, evicting the least
  recently written entry.
- A node that has successors but no known predecessor is no longer responsible for the whole
  ring: it forwards messages instead of holding them, and its responsibility interval is
  `(predecessor, self]` exactly.
- Node descriptors verify only when the body states the receiver's overlay, and onion backward
  payloads are verified under the receiver's overlay rather than the overlay stated by the exit.

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
