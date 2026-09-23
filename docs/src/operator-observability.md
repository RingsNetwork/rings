# Operator observability

A native Rings Node exposes a versioned operator surface on its **internal** listener:

- `GET /operator/v1/observability` returns a structured JSON snapshot.
- `GET /operator/v1/metrics` returns Prometheus text format.

Both routes inherit the internal API policy: the listener binds to loopback and every request
requires the node's Bearer token. The peer-facing external listener does not register these
routes. To expose metrics to a remote collector, put an authenticated, encrypted proxy in front
of the loopback listener; do not bind the peer-facing API as a metrics workaround.

```bash
TOKEN="$(tr -d '\r\n' < ~/.rings/api-token)"
curl --fail --header "Authorization: Bearer ${TOKEN}" \
  http://127.0.0.1:50000/operator/v1/observability
curl --fail --header "Authorization: Bearer ${TOKEN}" \
  http://127.0.0.1:50000/operator/v1/metrics
```

The `v1` path and JSON `schema_version` are compatibility boundaries. Fields may be added within
v1; incompatible meaning or removal requires a new versioned path.

## Scope and privacy

Message and lookup counters are node-local and begin when the current process starts. They are
not topic totals, network totals, or durable accounting. Recent message activity retains the last
256 completed operations in memory. Each record contains only time, action, finite traffic
category, compile-time message class, and outcome. Payloads, application topic strings, DIDs,
transaction IDs, transport addresses, delegation identifiers, and key material are excluded.

Prometheus labels use only finite action, outcome, and reliability enums. Peer DIDs appear only
in the authenticated JSON response, whose `peer_ratings` array is capped at 128 records. Never
turn those DIDs into metric labels in a collector.

## Values and alert semantics

| Signal | Type | Meaning |
| --- | --- | --- |
| `rings_message_operations_total` | counter | Successful sent, received, forwarded, and stored operations plus aggregate failures during this process. A send/forward succeeds when its next hop accepts the logical transfer. |
| `recent_messages` | bounded records | Oldest-first completion records for the last 256 message operations. `message_class` is a protocol variant, not an application-controlled topic. |
| `rings_session_key_valid` | gauge | Whether the active delegation is unexpired at scrape time. Expiry and remaining time contain no key identifier. |
| session rotation support and totals | gauge and counters | `rings_session_key_runtime_rotation_supported` is currently zero, both rotation counters remain zero, and JSON reports `runtime_rotation_supported: false`. A replacement delegation is loaded on restart; invalid delegation input prevents the API process from starting. |
| mailbox gauges | gauges | Live relay-inbox carriers and held-message elements retained by this node. Rings mailboxes are implicit DHT relay carriers rather than separately registered user accounts. |
| `rings_mailbox_stored_total` | counter | Successful offline-message holds during this process. Expiry/removal is reflected in the live gauges. |
| DHT lookup totals | counters | Successor and storage lookup rounds observed locally, split into success, explicit failure, and 30-second timeout. |
| `rings_dht_lookups_in_flight` | gauge | Correlations still awaiting a terminal observation, bounded to 1,024 entries. Correlation identifiers are never exported. |
| lookup latency | histogram | Local elapsed milliseconds from observed start to answer, failure, or timeout. |
| `rings_local_peer_ratings` | gauge | Count of retained peers in each local reliability class. This is this node's advisory assessment, never network-wide reputation. |
| `peer_ratings` | bounded records | Up to 128 DID-sorted local assessments with recent send/receive evidence and the local byte-credit score inputs. |
| `rings_process_api_healthy` | gauge | The authenticated HTTP handler answered and assembled its dependencies. |
| `rings_overlay_ready` | gauge | Conservative overlay readiness: at least one admitted peer and one DHT successor are present. It is deliberately distinct from process/API health. |

A basic alert set can page when `rings_process_api_healthy == 0`, warn when
`rings_overlay_ready == 0` for longer than the expected bootstrap window, warn before
`rings_session_key_seconds_remaining` reaches the operator's rotation lead time, and track
increases in lookup failure/timeout counters. Use rates over counters; process restarts reset them.

## JSON shape

The structured response groups `messages`, `session_key`, `mailboxes`, `dht_lookups`,
`peer_ratings`, and `health`. `counter_scope` is `node_local_process_lifetime`, and
`process_started_at_ms` makes counter resets explicit. Mailbox counts are built from the live DHT
storage view, which retires expired entries before counting them.
