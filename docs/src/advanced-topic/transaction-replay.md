# Destination-scoped transaction replay protection

Rings 0.24.0 gives a retained final destination an at-most-once dispatch guarantee for exact
signed transactions. The guarantee is receiver-local. It does not provide reliable delivery,
global ordering, exactly-once application effects, or replay recognition after replay state has
been deleted.

## Stream and transcript

Replay state is keyed by:

```text
StreamKey = (network_id, origin_delegator_did, destination_did, class)
```

The origin is recovered from `Transaction.verification.delegation.delegator_did()`. It is not the
delegatee DID and not an intermediate relay. Rotating a delegatee key therefore preserves
the same delegator-to-destination stream, while two destinations advance independently.

`class` is the traffic class (DHT control, storage, E2E, application) of the message the
transaction carries, derived from its signed data on both sides, so it adds no wire field (#898).
The outbound scheduler keeps order within a class lane only, with at most `OUTBOUND_LANE_WINDOW`
(8, below the 32-slot window) transactions of a lane in flight, and each class lane is pinned to
one ordered data channel of the connection:

```text
channel(lane) = pool[lane mod |pool|]
DHT control -> 0, storage -> 1, E2E -> 2, application -> 3
```

A class's transactions therefore reach the receiver in the order the lane sent them, and a
receive handler stalled on one channel holds only that channel's class. One stream per class
never rejects an honest sender's transaction as stale, however the lanes are interleaved; a
single stream shared by every class did, once one lane's backlog fell behind another's traffic,
and so would one class spread over several channels, once a stalled channel let later sequences
of the class overtake it by more than the window.

The order holds for frames that resolve on arrival. A frame whose delegation reference misses
(the receiver forgot the delegation to capacity eviction or expiry) is held for one repair round
trip and released independently of later frames, because the session link promises no order
among held frames (see [Delegation References](delegation-references.md)). If a window's worth
of its class is admitted meanwhile, the released frame is rejected as `Stale`: one frame lost
and counted, never a false admission. Removing this loss is tracked in #908.

This assumes a class's transactions reach the scheduler in signing order, as they do from one
sending task.

The per-class streams are stored one record per stream (see [Persistence and
bounds](#persistence-and-bounds)). A load that finds the former shared-stream snapshot
(`rings-core:transaction-replay`) deletes it without decoding it, which resets every replay window
once, as deleting the store does. The deletion is best effort: since the former snapshot is never
decoded, a failed deletion is counted as a persistence failure and logged, admission continues, and
the next load retries it; once it is gone, no load touches it again. An upgraded
sender's per-class sequences are stale to a node that has not upgraded, so the upgrade is
network-wide and mandatory.

Every transaction carries a mandatory `u64` sequence. The transaction signature transcript
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

At that destination, replay classification and the runtime-local origin rate quota share one
serialized admission boundary but remain separate state models. Replay, Fork, and Stale consume no
quota; quota rejection leaves the durable replay window unchanged; and a replay-store failure rolls
back the provisional quota reservation. Quota state is never encoded in a `ReplayRecord`. The
deterministic byte cost is the verified original transaction's `data.len()`: normal messages pay it
once, chunk envelopes pay nothing, and a successfully reassembled original pays it once before
logical lane admission.

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

Sender and receiver state are stored as one record per stream:

```text
rings-core:transaction-replay:stream:sender:<hex key>    ->  (key, last reserved sequence)
rings-core:transaction-replay:stream:receiver:<hex key>  ->  (key, 32-slot window)
```

A transition writes only its own stream's record, at most `TRANSACTION_REPLAY_RECORD_MAX_BYTES`
(1161 bytes: a 92-byte key and a 1066-byte window, with tag and length prefix), so the cost of an
admission does not grow with the number of streams retained. On load, every record must decode and
sit under its own key, or the stream it is filed as fails closed (see [Bad records](#bad-records)).
The native daemon opens the replay store as an authoritative file store (#909). On unix each write
is flushed to stable storage before its rename and the directory after it (`F_FULLFSYNC` on macOS),
so a crash leaves every record whole at its previous or its new value; on other targets a rename's
durability is the file system's own. A record file that cannot be read, that does not decode whole,
or that holds a whole record of another key is reported and never deleted. The file I/O runs on the
tokio blocking pool, so a flush never stalls an asynchronous worker.

One lock serializes all replay transitions and is held across each record write, so the store's
write latency bounds the node-wide transition rate. Each transition runs detached from its caller
on the runtime: lock, persist the stream's record, update the table, unlock. Cancelling a caller
abandons only its wait, so a record write is never left in flight while the next transition of its
stream runs, and a stale write can never land after a newer one. A cancelled caller's transition
therefore commits, exactly as if the caller had dropped the verdict: an admission enters the window
and is charged its quota though nobody dispatches it (a retransmission is then a `Replay`, and the
message is lost, as for any drop after admission), and a reservation consumes its sequences (a gap
the receiver accepts). A cancelled attempt is not shed from the queue: it still pays its flushed
write.

With the flushed native store that bound is the
flush latency. `test_durable_throughput_of_replay_admissions` (ignored by default; run it with
`--ignored --nocapture`) measures it. On an Apple M1 Max SSD (APFS) it measured:

| Store | Admissions/s | p50 | p99 |
|---|---|---|---|
| authoritative file | 49–57 | ~17–18 ms | ~30–47 ms |
| disposable file | ~4,500 | ~0.2 ms | ~0.4 ms |
| memory | ~97,000 | ~10 µs | ~17 µs |

The benchmark admits through the production path (`admit_with_quota`), so each figure includes
the detached task every transition runs on.

Sender reservations pay the same cost. The ceiling is far below the outbound lane rate, and group
commit is tracked in #916.

Each table retains at most `TRANSACTION_REPLAY_STREAM_CAPACITY` = 4 x 4096 streams: every class
stream of 4096 account-destination pairs, or more pairs that use fewer classes. Each receiver stream
has exactly 32 hot digest slots. A full store is at most `TRANSACTION_REPLAY_STORE_MAX_RECORDS`
(32,768) records and `TRANSACTION_REPLAY_STORE_MAX_BYTES` (28,295,168 bytes). New streams fail
closed at the bound; there is no LRU eviction or sender-controlled reset. The native daemon keeps
the records in a dedicated 40 MiB authoritative file store, which evicts nothing: a write beyond its
budget fails with `StorageBudgetExhausted`, and so does an open under a lowered budget. Browser
providers keep them in a dedicated IndexedDB store with one row more than the record bound, which
evicts beyond its limit, and an evicted record would reopen replay for its stream. Both are sized
never to reach their limits. A custom `SwarmBuilder` or `ProcessorBuilder` must supply durable
`ReplayStorage` to retain the restart guarantee; their in-memory default guarantees replay rejection
only for the lifetime of that runtime.

Deleting or replacing the replay store deletes the guarantee for its streams. There is no safe
incarnation/reset protocol in 0.24.0, so operators must retain the store across restarts and fail
closed on storage errors. Counters expose `Replay`, `Fork`, `Stale`, persistence failures,
unrestorable records and the calls refused on their streams.

## Bad records

The first replay operation restores the store from one scan and caches the result; no later call
reads the store again. A record restores its stream iff it decodes, holds a valid window and sits
under the record key of the stream it carries. Any other record is kept and joins the set `U` of
unrestorable records (#910): torn, corrupt, unreadable (any read error but the absence of the entry,
such as a permission error, an I/O error, a dangling link, or a directory in the record's place),
holding another key's record (copied, moved or renamed there from outside the store, or a bit flip
in its key text), misplaced, or holding an invalid window. Its name is the storage's record name:
the native file name, or the browser row key. A directory or a symbolic link to one in a record's
place charges nothing against the store's budget.

```text
unavailable(table, key)  ⟺  record_name(record_key(table, key)) ∈ U
```

- An unavailable stream refuses every reservation and admission with
  `TransactionReplayStreamUnavailable { key, record }`. No replay is admitted from, and no sequence
  is reused by, a stream whose record is torn, corrupt or unreadable.
- Every other stream already in the store restores and runs exactly as it would without the bad
  record. A new stream is affected only by the bound: each unrestorable record holds a slot of both
  tables' stream bounds.
- Restore costs one read of the whole store, once. Each later call reads nothing and writes one
  record.
- Each unrestorable record is logged once at load and counted in `unrestorable_record`. Each refused
  call is counted in `unavailable_stream`.

A scan fails as a whole only when the store itself cannot be listed, for example when its index
lock is poisoned. Such a scan restores nothing: it is counted as a persistence failure, the call
fails closed, and the next call scans again.

The law covers the records the store holds. A record that is absent is indistinguishable from a
stream never seen, and its stream restarts from `First` (tracked in #915). Records go absent when
they are deleted, moved or renamed away (the file they land in is undecodable for its own name,
so the stream that name denotes fails closed), when a file name is damaged, or when a rename is
rolled back on a target where the directory cannot be flushed.

### Clearing one stream

An operator clears a failed stream, and only that stream, by removing its record with the node
stopped and then starting the node:

1. Take the record name from the refusal (`record`) or from the load's log line (`record`, in
   `replay record does not restore; its stream fails closed until it is cleared`).
2. Stop the node. On native, stop the daemon. In a browser, close every tab of the origin that runs
   the node, then open one tab of the origin with the node not started.
3. Delete the record:
   - On native, delete the file of that name in the `transaction-replay` directory beside the data
     store.
   - In a browser, open DevTools, go to Application, then IndexedDB, and select the database
     `<storage name>/transaction-replay`; for the default provider this is
     `rings-node/transaction-replay`. In its object store of the same name, delete the row whose
     key is the record name.
4. Start the node. The stream starts again from `First`.

This resets that stream's replay window alone, and no other stream's state changes.
- **Cleared receiver stream.** An unexpired transaction of the stream may be admitted once more.
- **Cleared sender stream.** The stream restarts at sequence zero. Its destination rejects those
  sequences until they pass the destination's retained high watermark:
  - below the window, as `Stale`;
  - inside the window, as `Fork`, with a signed `TransactionForkEvidence` against this node,
    because the same sequences now carry new transactions.

  The messages carried by the rejected sequences are lost.

## Hard cutover

0.24.0 was a network-wide protocol cutover: the sequence field became mandatory and the
transaction signing domain and payload marker changed with it. 0.28.0 is the next one: the
signing domain is `rings-core:message-verification:transaction`, the payload wire encoding
begins with the `RINGS-PAYLOAD` marker, and the replay store key is
`rings-core:transaction-replay` (see [Delegation References](delegation-references.md)). Payloads
without the current marker are rejected before deserialization. There is no dual decoder,
negotiation, feature flag, downgrade path, or legacy fallback, and no version behind any of these
names: the protocol is not versioned before 1.0. Mixed-version overlays are unsupported. Per-class
streams (#898) are the next cutover: the store becomes one record per stream under
`rings-core:transaction-replay:stream:`, and every node, seeds included, upgrades together.

## Non-guarantees

- No cross-account or global order.
- No claim that a sequence gap identifies loss or a dishonest relay.
- No retransmission, acknowledgement, or head-of-line blocking protocol for transactions (the
  link's session confirmations and repairs are about delegation references, not about
  transactions).
- No exactly-once application side effects.
- No recognition of messages older than a deliberately deleted replay store.
- No reputation or slashing consequence for fork evidence.
