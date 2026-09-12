# Destination-scoped transaction replay protection

Rings 0.24.0 gives a retained final destination an at-most-once dispatch guarantee for exact
signed transactions. The guarantee is receiver-local. It does not provide reliable delivery,
global ordering, exactly-once application effects, or replay recognition after replay state has
been deleted.

## Stream and transcript

Replay state is keyed by:

```text
StreamKey = (network_id, origin_account_did, destination_did)
```

The origin is recovered from `Transaction.verification.session.account_did()`. It is not the
delegated session DID and not an intermediate relay. Rotating a session key therefore preserves
the same account-to-destination stream, while two destinations advance independently.

Every transaction carries a mandatory `u64` sequence. The transaction v2 signature transcript
binds the receiver-selected `network_id` through the signing domain and binds `destination`,
`tx_id`, `sequence`, and `data` through the transaction hash. `ts_ms` and `ttl_ms` remain session
authorization and message-liveness inputs; neither is used to order replay state.

## Receiver state machine

One stream retains a fixed 32-slot window:

```text
SequenceState {
    high: u64,
    accepted: [Option<TransactionDigest>; 32],
}
```

`observe(state, sequence, digest)` is deterministic and returns one typed verdict:

- `First` establishes a baseline when no retained stream exists.
- `Advance` moves the window to a higher sequence. A gap is allowed and is not evidence of
  omission.
- `Late` fills an empty slot inside the retained window.
- `Replay` rejects the same digest at an occupied sequence.
- `Fork` rejects a different digest at an occupied sequence and reports both digests.
- `Stale` rejects a sequence below the retained window.

The final destination applies this transition after both transaction and payload signatures
verify and before application validation, handler effects, or the application inbound callback.
An intermediate Chord relay does not read or advance origin replay state. Chunk envelopes retain
their existing bounded reassembly/tombstone replay defense and do not advance logical transaction
replay state. Each envelope binds the already-reserved logical sequence without consuming another
sequence; after reassembly, the original logical transaction is admitted only if that node is its
final destination.

## Persistence and bounds

The sender reserves and persists the next sequence before exposing a signature. A crash may leave
a gap but cannot reuse a successfully reserved sequence. A reservation that would wrap `u64`
fails closed.

One runtime serializes all writers to its allocator. Multiple devices or processes using the same
account and destination must delegate signing to that one allocator; merely pointing independent
runtimes at the same key-value store is not an atomic multi-writer protocol. If they do not
coordinate, the destination reports their conflicting sequence as `Fork`. Restoring an identity
backup without its allocator state has the same limitation and is unsupported.

The receiver persists every admitted transition before application validation or dispatch. A
crash after persistence but before dispatch may lose the event. This is the deliberate
persistence-before-dispatch boundary: it prevents duplicate dispatch but is not exactly-once
execution because replay storage and application handlers do not share a transaction.

Sender and receiver state are stored in one versioned snapshot. Each table retains at most 4096
streams, and each receiver stream has exactly 32 hot digest slots. New streams fail closed at the
bound; there is no LRU eviction or sender-controlled reset. The native daemon keeps the snapshot
in a dedicated 16 MiB atomic file store, and browser providers keep it in a dedicated IndexedDB
store. A custom `SwarmBuilder` or `ProcessorBuilder` must supply durable `ReplayStorage` to retain
the restart guarantee; their in-memory default guarantees replay rejection only for the lifetime
of that runtime.

Deleting or replacing the replay store deletes the guarantee for its streams. There is no safe
incarnation/reset protocol in 0.24.0, so operators must retain the store across restarts and fail
closed on storage errors. Counters expose `Replay`, `Fork`, `Stale`, and persistence failures.

## Hard cutover

0.24.0 is a network-wide protocol cutover. The sequence field is mandatory, transaction
signatures use the `rings-core:message-verification:transaction:v2` domain, and the payload wire
encoding begins with the `RINGS-TX-V2` marker. Unprefixed 0.23.x payloads are rejected before
deserialization. There is no dual decoder, negotiation, feature flag, downgrade path, or legacy
fallback. Mixed-version overlays are unsupported.

## Non-guarantees

- No cross-account or global order.
- No claim that a sequence gap identifies loss or a dishonest relay.
- No retransmission, acknowledgement, or head-of-line blocking protocol.
- No exactly-once application side effects.
- No recognition of messages older than a deliberately deleted replay store.
- No reputation or slashing consequence for fork evidence.
