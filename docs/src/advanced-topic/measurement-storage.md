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

An IndexedDB row stores `key`, `data`, and `last_visit_time`. A successful `get`
updates the timestamp in the **same read-write transaction** as its lookup and
awaits commit before returning. Splitting the touch from the read could overwrite
a concurrent value; removing the touch would change least-recently-used eviction.
A missing-key read also waits for transaction completion. Access timestamps remain
monotonic per key when the clock stalls or moves backward, saturating at `i64::MAX`.
This is the existing per-key ordering guarantee, not a global logical clock.

`visit_count` and `created_time` are no longer serialized, and newly created
stores have only the `last_visit_time` index. Database opening supplies no version:
Rexie runs schema creation only for a new database. An existing database therefore
retains its unused `visit_count` index. Normal deserialization ignores surplus row
fields and the next successful touch rewrites the current row shape; no migration
branch or version family is introduced. Removing an existing index would require
an IndexedDB schema upgrade, so this cleanup does not promise physical removal of
that old index and never deletes a user's database.

The constructor requires an explicit database name and nonzero capacity. Raw
transactions are private to the adapter. Browser tests use unique database names,
including an isolated previous-schema fixture, and exercise actual IndexedDB reads,
writes, errors, timestamp touches, capacity eviction, and reopening.
