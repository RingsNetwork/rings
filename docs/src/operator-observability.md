# Status observability

Rings Node exposes operator observations through its existing authenticated status route:

```bash
TOKEN="$(tr -d '\r\n' < ~/.rings/api-token)"
curl --fail --header "Authorization: Bearer ${TOKEN}" \
  'http://127.0.0.1:50000/status?view=observability'
```

The compact projection is available on both the internal and external listeners because both
already serve `/status`. It inherits their existing Bearer-token requirement. This change adds no
new HTTP route and no unauthenticated surface.

## Why the projection is separate from the default body

`GET /status` keeps its existing response shape for compatibility. That legacy response is a full
inspection: besides peers and DHT topology, it serializes every persistent and cache storage value.
Its size is therefore data-dependent and may approach the configured storage capacity; the default
native data-storage capacity is 200,000,000 bytes before JSON overhead. Do not poll the full view as
a lightweight health check.

`GET /status?view=observability` avoids the full storage inspection. It returns only the node
version plus a bounded `observability` object. The largest collections are capped at 32 recent
message records and 32 peer-rating records. A conservative maximum-width production fixture is
about 18 KiB and is kept below 32 KiB by a regression test. This makes the projection suitable
for periodic polling without adding the storage payload or performing the legacy full status
serialization.

## Polling interval and request cost

Poll `GET /status?view=observability` at most once every 60 seconds per node. Do not use it for
high-frequency polling, do not start a new request while the previous request is still running,
and avoid having multiple independent collectors poll the same node. Configure a client timeout so
a slow request cannot accumulate overlapping work.

The response size is bounded, but the work needed to assemble a current snapshot is not constant.
Each request walks the live persistent DHT storage view to count mailbox carriers and held messages;
the walk reads and decodes stored entries and retires expired entries. Snapshot assembly also
projects the current retained peer measurements. Its CPU, storage I/O, allocation, and lock-hold
costs therefore grow with the node's stored data and retained peer state. The 60-second interval is
an operational recommendation intended to limit this work; it is not a claim that the endpoint has
constant-time cost.

Do not periodically poll the default `GET /status` response. It additionally serializes every
persistent and cache storage value and is intended for explicit inspection rather than monitoring.

The JSON object's `schema_version` is the compatibility boundary. Fields may be added within the
current version; incompatible meaning or removal requires a schema-version increment.

## Scope and privacy

Message and lookup counters are node-local and begin when the current process starts. They are not
topic totals, network totals, or durable accounting. Recent message activity retains the last 32
completed operations in memory. Each record contains only sequence, time, action, finite traffic
category, compile-time message class, and outcome. Payloads, application topic strings, DIDs,
transaction IDs, transport addresses, delegation identifiers, and key material are excluded.

Peer DIDs appear only in the authenticated `peer_ratings` array, which is sorted and capped at 32
records. Use the existing paginated `listPeerMeasurements` RPC when every retained peer is needed.
Lookup correlation identifiers and mailbox identifiers are never exported.

## Values and alert semantics

| Field | Kind | Meaning |
| --- | --- | --- |
| `messages` | counters | Successful sent, received, forwarded, and stored operations plus aggregate failures during this process. A send or forward succeeds when its next hop accepts the logical transfer. |
| `recent_messages` | bounded records | Oldest-first completion records for the last 32 message operations. `message_class` is a protocol variant, not an application-controlled topic. |
| `session_key` | gauges and counters | Active delegation validity, creation, expiry, and remaining time. Runtime rotation is currently unsupported and its counters remain zero; a replacement delegation is loaded on restart. |
| `mailboxes` | gauges and counter | Live relay-inbox carriers and held messages retained by this node, plus successful offline-message holds during this process. |
| `dht_lookups` | counters, gauge, histogram | Successor and storage lookup rounds observed locally, split into success, explicit failure, and 30-second timeout, with bounded in-flight state and cumulative latency buckets. |
| `peer_ratings` | bounded records | Up to 32 DID-sorted local assessments with recent send/receive evidence and local byte-credit score inputs. Use `listPeerMeasurements` for the paginated complete set. These are advisory local assessments, not network-wide reputation. |
| `health.process_api_healthy` | gauge | The authenticated handler answered and assembled its dependencies. |
| `health.overlay_ready` | gauge | Conservative overlay readiness: at least one admitted peer and one DHT successor are present. It is deliberately distinct from process/API health. |

`counter_scope` is `node_local_process_lifetime`, and `process_started_at_ms` makes resets explicit.
Use deltas or rates over cumulative counters because every process restart resets them. Mailbox
counts come from the live DHT storage view, which retires expired entries before counting them.
