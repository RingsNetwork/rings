# Local measurements and browser storage

`rings-measure` owns authenticated useful-byte credits and recent reliability
windows. Core's `Measure` boundary requires explicit `record` and atomic
`record_batch` implementations. There is no counter-only fallback: callers supply
an `Authentication` and a `MeasurementEvent`, including useful bytes. A failed
batch must leave the ledger unchanged. Node's `PeriodicMeasure` applies the whole
batch under its runtime lock, then wakes coalesced persistence.

Each retained `PeerMeasurement` includes a `CreditRecord`. Absence of a peer is
represented by the outer optional measurement, not by missing credit within an
existing peer. The protobuf RPC message field retains its generated presence
wrapper and is populated for every projected peer. Queries still include retained
disconnected peers; bounded pagination, `peer_measurements()`, credit scores, and
measurement RPCs remain available. Reliability classification requires an explicit
`ReliabilityPolicy`; thresholds alone do not specify minimum positive evidence.

Authentication and logical completion remain the recording boundaries. A locally
selected DID may only contribute a failed-send observation to an already retained
peer; it cannot create credit or refresh authenticated last-seen time. Chunking
must produce one terminal logical outcome with exact useful bytes. Provisional
service receipts use their separate evidence store and do not alter credit or
reliability.

## IndexedDB access and eviction

An IndexedDB row stores `key`, `data`, and `access_stamp`. Recency is a
store-wide **logical clock**, not wall-clock time: each database keeps one access
counter in a companion object store, and every `put` and every successful `get`
takes the next counter value as the row's stamp and advances the counter in the
**same read-write transaction** as the row write, awaiting commit before
returning. Splitting the touch from the read could overwrite a concurrent value;
removing the touch would change least-recently-used eviction. A missing-key read
is not an access and also waits for transaction completion.

The law: k accesses receive k distinct, strictly increasing stamps, whatever the
browser's timer resolution; no timer is read. Eviction removes the rows with the
smallest stamps through the `access_stamp` index, so two rows never tie. `clear`
keeps the counter, so a stamp is never reused within one database. The counter
stops with an error at `Number.MAX_SAFE_INTEGER` rather than repeat a stamp.

Schema version 2 introduced the counter. A version-1 database, whose rows carry
wall-clock `last_visit_time`, is migrated in place and never cleared: the upgrade
adds the counter store and the `access_stamp` index and drops the old
`last_visit_time` and `visit_count` indexes; the first open then restamps every
row `0..n` in its former eviction order (`last_visit_time`, then key) and sets the
counter to `n` in one transaction, dropping the unused `visit_count` and
`created_time` fields. The counter record marks a completed migration; an
interrupted one reruns from the untouched rows on the next open. The upgrade
waits until every connection still open at version 1, such as another tab
running an older build, has closed.

The constructor requires an explicit database name and nonzero capacity. Raw
transactions are private to the adapter. Browser tests use unique database names,
including an isolated previous-schema fixture, and exercise actual IndexedDB reads,
writes, errors, access-clock touches, capacity eviction, migration, and
reopening. No browser test reads or waits on wall-clock time.
