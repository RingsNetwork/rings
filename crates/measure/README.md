# rings-measure

Pure local peer credit and transport reliability accounting for Rings.

`rings-measure` separates two local relations and keeps side effects (clocks, persistence, locking, and network I/O) outside the crate:

- **Byte credit (`CreditRecord`, `CreditPolicy`, `CreditScore`)**: tracks authenticated useful bytes exchanged with a peer and computes a bounded aMule-compatible credit multiplier in `[1.0, 10.0]` (`CreditPolicy::amule()` defaults to a `1_000_000`-byte activation threshold and `150`-day retention window).
- **Advisory reliability (`ReliabilityWindow`, `ReliabilityPolicy`, `ReliabilityThresholds`, `ReliabilityClass`)**: summarizes recent transport outcomes inside an aligned non-zero time window (`UnixTime`) for advisory peer ordering via `order_peers_by_reliability`.
- **State machine (`MeasurementLedger`, `MeasurementEvent`, `MeasurementBatch`, `MeasurementSnapshot`)**: applies explicitly attributed peer events (`Authentication`) and exports bounded snapshots (`MEASUREMENT_SNAPSHOT_VERSION`).
- **Provisional service receipts (`ProvisionalEvidenceStore`, `EvidenceSnapshot`, `EvidenceLimits`)**: stores bounded canonical service-receipt bytes keyed by `EvidenceDigest` and `EvidenceAccountPair` with independent replay markers (`EVIDENCE_SNAPSHOT_VERSION`).
