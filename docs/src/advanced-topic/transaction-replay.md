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

No frame is held on the way (#908): a frame whose delegation reference misses is dropped, never
delivered late, so the frames of one lane reach admission in arrival order. An honest sender's
reference resolves whenever its transit stays under two delivery bounds, and a frame that waits
in its lane at the receiver still resolves however long it waited, so a miss needs a late frame
and a full table at once (see [Delegation References](delegation-references.md)).

This assumes a class's transactions reach the scheduler in signing order, as they do from one
sending task.

The per-class streams are stored one record per stream (see [Persistence and
bounds](#persistence-and-bounds)). A store of 0.32 or earlier, which holds one snapshot of streams
shared by every class under `rings-core:transaction-replay`, is not converted: the operator deletes
the replay store while the node is stopped before upgrading, which resets every replay window once.
An upgraded sender's per-class sequences are stale to a node that has not upgraded, so the upgrade
is network-wide and mandatory.

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
verify and before handler effects or the application inbox.
An intermediate Chord relay does not read or advance origin replay state. Chunk envelopes retain
their existing bounded reassembly/tombstone replay defense and do not advance logical transaction
replay state. Each envelope binds the already-reserved logical sequence without consuming another
sequence; after reassembly, the original logical transaction is admitted only if that node is its
final destination.

At that destination, replay classification and the runtime-local origin rate quota share one
serialized admission boundary but remain separate state models. Replay, Fork, and Stale consume no
quota; quota rejection leaves the durable replay window unchanged; and a replay-store failure rolls
back the provisional quota reservation. The quota clock is read inside that boundary, so the
instants a quota record receives follow the order in which admissions hold it and never regress.
Quota state is never encoded in a `ReplayRecord`. The
deterministic byte cost is the verified original transaction's `data.len()`: normal messages pay it
once, chunk envelopes pay nothing, and a successfully reassembled original pays it once before
logical lane admission.

## Persistence and bounds

The sender never exposes a signature over a sequence its durable record does not cover. It
persists a lease, not each sequence (#1043): a reservation that outruns the stream's lease writes
a new one covering at least the next `TRANSACTION_SEQUENCE_LEASE` (64) sequences, and the
reservations inside it are handed out from memory without a write. A stream reserving one sequence
per transaction therefore writes once per 64 transactions. A restart resumes past the durable
lease, so a crash may leave a gap, of at most 64 sequences for single reservations, but never
reuses a sequence. The receiver takes the gap as one `Advance`: no transaction carries a skipped
sequence, since none was signed. The laws, with their proof sketch over every crash point, are
in `crates/core/src/message/replay/lease.rs`. A reservation that would wrap `u64` fails closed.

One runtime serializes all writers to its allocator. Multiple devices or processes using the same
account and destination must delegate signing to that one allocator; merely pointing independent
runtimes at the same key-value store is not an atomic multi-writer protocol. If they do not
coordinate, the destination reports their conflicting sequence as `Fork`. Restoring an identity
backup without its allocator state has the same limitation and is unsupported.

The receiver persists every admitted transition before application validation or dispatch. A
crash after persistence but before dispatch may lose the event. This is the deliberate
persistence-before-dispatch boundary: it prevents duplicate dispatch but is not exactly-once
execution because replay storage and application handlers do not share a transaction.

Link-local transactions are the exception (#1081). A transaction is link-local when all three
hold: it is Application traffic, it arrived on the authenticated connection of its own origin,
and its namespace was registered in advance as delegating its replay (a protocol's
`delegates_replay`, published with `Swarm::set_replay_delegation`). Core decides this from the
envelope and the connection; it never calls the application. Such a transaction is admitted in a
volatile window of its stream on that connection. The window is never written, and it is released
with the connection, so it spans one link generation. Every other transaction, a relayed copy
included, keeps the durable window. A namespace that declares delegated replay must act on each
of its payloads at most once by its own replay key, across connections, generations, envelopes
and restarts. The onion data plane does, with its replay store and process epoch, and it is the
one namespace that declares it. Only the origin can present a link-local transaction again: its
link is authenticated and encrypted, and the transaction names this node as its destination. So
the durable window protected nothing there that the declaring namespace does not. The law is the
*Link-local transactions* section of `papers/durable-records.tex`.

Sender and receiver state are stored as one record per stream:

```text
rings-core:transaction-replay:stream:sender:<hex key>    ->  (key, last leased sequence)
rings-core:transaction-replay:stream:receiver:<hex key>  ->  (key, 32-slot window, relay custody,
                                                             introducer, spent instant)
```

A transition writes only its own stream's record, at most `TRANSACTION_REPLAY_RECORD_MAX_BYTES`
(5089 bytes: a 92-byte key, a 1066-byte window, a relay custody of up to 64 deliveries of 61
bytes and a 23-byte charge, with tag and length prefixes), so the cost of an admission does not grow with the number of
streams retained. On load, every record must decode and
sit under its own key, or the stream it is filed as fails closed (see [Bad records](#bad-records)).
The native daemon opens the replay store as an authoritative file store (#909). On unix each write
is flushed to stable storage before its rename and the directory after it (`F_FULLFSYNC` on macOS),
so a crash leaves every record whole at its previous or its new value. Windows cannot flush a
directory, so there every rename and removal of the store is write-through (`MoveFileExW` with
`MOVEFILE_WRITE_THROUGH`, #915); on any other target a rename's durability is the file system's
own. A record file that cannot be read, that does not decode whole,
or that holds a whole record of another key is reported and never deleted. The file I/O runs on the
tokio blocking pool, so a flush never stalls an asynchronous worker.

Browser providers open the replay store as an authoritative IndexedDB store, which commits every
read-write transaction with `durability: "strict"` and reports a write only on its `complete`
event (#912). The default hint does not survive a system crash in any engine: Chromium's default
is relaxed since version 121, and Gecko and WebKit run SQLite in WAL mode without a flush per
commit. `"strict"` is each engine's strongest commit; whether it also flushes a drive's write cache
is the engine's choice. An IndexedDB transaction is atomic, so a crash leaves every record whole at
its previous or its new value.

One lock serializes all replay transitions, but no record write runs under it: replay commits by
group commit (#916). A transition locks, applies its effect to the in-memory tables, stages its
stream's record into the open batch, unlocks, and waits until that batch is durable. One writer at a
time writes the batches, in order, outside the lock. The transitions that run while one batch is
written gather into the next, and one store batch writes all of them:

- On the native store, every record goes to its own temporary file, and each file is `fsync`ed. One
  `F_FULLFSYNC` barrier on Apple targets then makes them all durable together. After that come the
  renames, and one directory flush covers all of them.
- On IndexedDB, the batch is one strict read-write transaction.

The laws, stated in `crates/core/src/message/replay/commit.rs` and in the Group commit section of
`papers/durable-records.tex`:

- **Release.** No result is returned before its batch is durable. That holds for a refusal too,
  and for a reservation inside its lease that writes nothing.
- **Atomicity.** Each record is whole at its previous or its new value after any crash.
- **Order.** Each stream's durable values follow lock order.
- **Fail closed.** A failed batch fails, and undoes, every pending transition: its own, and those of
  the batch gathering on top of it. It is counted once as a persistence failure.
- **Latency.** A transition waits for at most two batch writes: the one in flight, then its own.

Each transition runs detached from its caller on the runtime, so cancelling a caller abandons only
its wait, never a staged effect. A cancelled caller's transition therefore commits, exactly as if
the caller had dropped the verdict: an admission enters the window and is charged its quota though
nobody dispatches it (a retransmission is then a `Replay`, and the message is lost, as for any drop
after admission), and a reservation consumes its sequences (a gap the receiver accepts). A crash
after some records of a batch landed and before the batch was released is the same case.

`test_durable_throughput_of_replay_admissions` (ignored by default; run it with
`--release --ignored --nocapture`) measures both latency and throughput. One admission at a time
pays a whole batch write. Admissions that run concurrently share batches, so the rate grows with
the transitions in flight. One run on an Apple M1 Max SSD (APFS) measured:

| Store | Admissions/s | p50 | p99 |
|---|---|---|---|
| authoritative file, one at a time | 39 | ~24 ms | ~46 ms |
| authoritative file, 64 in flight | 922 | | |
| disposable file, one at a time | ~4,000 | ~0.2 ms | ~0.5 ms |
| memory, one at a time | ~67,000 | ~11 µs | ~41 µs |

Before group commit the authoritative store sustained 49–57 admissions/s whatever the concurrency.
The benchmark admits through the production path (`admit_with_quota`), so each figure includes the
detached task every transition runs on. Sender reservations write once per lease of 64 sequences,
so a node's replay writes are about one per admission, and they share the same batches.

Each table retains at most `TRANSACTION_REPLAY_STREAM_CAPACITY` = 4 x 4096 streams: every class
stream of 4096 account-destination pairs, or more pairs that use fewer classes. Each receiver stream
has exactly 32 hot digest slots. A full store is at most `TRANSACTION_REPLAY_STORE_MAX_RECORDS`
(32,768) records and `TRANSACTION_REPLAY_STORE_MAX_BYTES` (28,692,288 bytes, counting the relay
custody once for the whole store, since it holds at most 64 deliveries). The sender table holds
this node's own streams, and a new one fails closed at the bound. The receiver table is shared by
edge, as the next section describes. There is no LRU eviction or sender-controlled reset. The
native daemon keeps
the records in a dedicated 40 MiB authoritative file store, which evicts nothing: a write beyond its
budget fails with `StorageBudgetExhausted`, and so does an open under a lowered budget. Browser
providers keep them in a dedicated authoritative IndexedDB store with a row capacity of
`TRANSACTION_REPLAY_STORE_MAX_RECORDS`, which evicts nothing either: a new record beyond it fails
with `StorageBudgetExhausted`, and so does an open under a lowered capacity (#923). Both are sized
never to reach their limits, so the refusal is a bound, not an expected path. A custom
`SwarmBuilder` or `ProcessorBuilder` must supply an authoritative `ReplayStorage`. Without one,
the node refuses every reservation and admission with `TransactionReplayStorageUnavailable`, so
it neither sends nor accepts a transaction (#1022): an in-memory store would lose every stream's
window at each restart, reopening replay on the receiver and re-signing used sequences on the
sender.

Deleting or replacing the replay store deletes the guarantee for its streams. There is no safe
incarnation/reset protocol in 0.24.0, so operators must retain the store across restarts and fail
closed on storage errors. Counters expose `Replay`, `Fork`, `Stale`, persistence failures,
unrestorable records and the calls refused on their streams.

## Receiver streams by connection slot

An origin is a key pair, free to mint, so a bound per origin bounds nothing: before #907 anyone could
fill the receiver table with one transaction from each of 16,384 fresh origins and refuse every new
origin for good. The receiver table is now accounted by connection slot (the load-bounds paper,
Law 2). A node has `R = 2(160 + s + 1)` slots, 328 at the default three successors; an admitted
connection takes the cleanest free slot (the one whose items use the least of their floor) and
frees it when it is retired. A new stream is charged to
its introducer:

- the slot of the handshake-authenticated connection that delivered its first transaction;
- the inbox, for mail out of this node's own relay inbox;
- this node itself, for out-of-band connection signaling.

Each slot has a floor of 32 new streams. Slots past their floors share a pool of 3328, each up to
its fair claim `⌈3328 / n⌉`, where `n` counts the over-floor slots. The inbox has a share of 768
and this node a share of 1024, so a flood of mail never refuses the node's own signaling. At the
default three successors the table holds at most `328 x 32 + 3328 + 1024 + 768 = 15,616`
streams, and 64 more per extra successor: the shares stay within the table's hard capacity of
16,384 up to 15 successors, and past that the hard capacity refuses first. A new stream past its introducer's share fails with
`TransactionReplayStreamCapacityExceeded`, naming the introducer and the bound. A party with many
origins and `k` slots can be refused only on those `k` slots.

A stream stays charged to its slot after its connection leaves, so rotating connections never
grows the table past its bound. The cost is that a connection admitted into a slot inherits what
the slot still holds. Because a newcomer takes the cleanest free slot, a party that rotates
connections reaches it only by charging every free slot, which leaves the newcomer with only its
fair claim until those streams are spent (the paper's rotation residual, #1094). A connection
that keeps its slot is never affected.

A stream leaves the table once it is spent: every transaction it admitted can no longer be
delivered, by a clock up to `σ` behind.

- A control, storage or E2E stream is spent `σ` after its last proof expires, about 10 minutes after
  its last default-lifetime transaction.
- An application stream is spent another `MAX_RELAY_INBOX_TTL_MS + σ` later (7 days), since its
  mail may still arrive out of a relay inbox, judged as of its hold instant.

The next admission expires every relay custody entry whose transaction is spent, then retires every
spent stream and removes its record. A custody entry is spent no later than its stream, so a spent
stream holds no custody by then.

A relay custody entry (sequence, digest, spent instant) leaves custody only when the application
takes its mail, or once its transaction is spent and no carrier can deliver it again. A drain pass
that reads no carrier, or a carrier without the element, removes nothing: the read may come from a
holder that a membership change just named, before its hand-off landed, and dropping custody on
its word would refuse the hand-off's redelivery after a restart as a replay (#1059).

The cost is liveness. An entry whose mail was admitted, not taken, and then lost at every member of
its chain (a chain that churned out without a hand-off) stays until its transaction is spent, up to
the proof's lifetime plus 7 days after its admission. The bound of 64 entries is shared by the whole
store, so with 64 such entries every new relay delivery to the node, from every origin, waits
(`RelayCustodyFull`, the pass ends and the mail stays stored) until the earliest of them is spent.
A relay inbox that stays undelivered while mail is held for the node points to this stall. It ends
by itself at that instant; removing the `transaction-replay` store ends it at once, at the cost of
the replay guarantee (see [Bad records](#bad-records) for that reset).
Forgetting a spent stream reopens no replay, so a refusal ends once a stream of the refused edge is
spent. Each receiver record carries its introducer and spent instant, so a restart recovers the
accounting.

## Bad records

The first replay operation restores the store from one scan and caches the result; no later call
reads the store again. Every record is written under the record key of the stream it carries, and
a record restores its stream iff it decodes and holds a valid window. Any other record is kept and
joins the set `U` of unrestorable records (#910): torn, corrupt, unreadable (any read error but the
absence of the entry, such as a permission or an I/O error), holding another key's record (which
only something outside the store can put there), or holding an invalid window. A record the native
store marked present and no longer holds joins `U` too, as missing (#915). Its name is the
storage's record name: the native file name, or the browser row key.

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

The law covers the records the store witnesses (#915). The native store keeps an empty presence
marker `<name>.p` beside every record it made durable: it creates the marker only after the
record's rename is flushed, removes it before the record, and marks any unmarked record at open (a
crash between a rename and its marker). A marker whose record is gone is a record removed, renamed
or lost outside the store, and its stream fails closed as missing instead of restarting from
`First`. A file renamed to upper case is never a record name: on a case-sensitive volume its
stream is missing, and on a case-insensitive one (default APFS) the store still reads the file
under its own name. The browser store needs no marker: an IndexedDB database loses no proper
subset of its rows, and the store evicts nothing (#923).

A store lost whole (the native `transaction-replay` directory, or the browser database or origin)
is indistinguishable from a store never written, since its markers go with it. That is the reset
of every stream described under [Clearing one stream](#clearing-one-stream).

### Clearing one stream

An operator clears a failed stream, and only that stream, by removing its record with the node
stopped and then starting the node:

1. Take the record name from the refusal (`record`) or from the load's log line (`record`, in
   `replay record does not restore; its stream fails closed until it is cleared`).
2. Stop the node. On native, stop the daemon. In a browser, close every tab of the origin that runs
   the node, then open one tab of the origin with the node not started.
3. Delete the record:
   - On native, delete the file of that name and its presence marker `<name>.p` in the
     `transaction-replay` directory beside the data store. Deleting the file alone leaves the
     stream failed closed, as missing.
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
`rings-core:transaction-replay:stream:`, and every node, seeds included, upgrades together. Edge
accounting (#907) changes the receiver record, which gains its charge: the operator removes the
`transaction-replay` store while the node is stopped (the documented reset), since a record
without a charge does not restore.

## Non-guarantees

- No cross-account or global order.
- No claim that a sequence gap identifies loss or a dishonest relay.
- No retransmission, acknowledgement, or head-of-line blocking protocol for transactions (the
  link's leases and disclaimers are about delegation references, not about transactions).
- No exactly-once application side effects.
- No recognition of messages older than a deliberately deleted replay store.
- No reputation or slashing consequence for fork evidence.
