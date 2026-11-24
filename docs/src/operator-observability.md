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

Use the pull-based `/status?view=observability` endpoint (or external Prometheus exporters fed
by it — `rings` does not serve a built-in `/metrics` route) for zero-overhead local health and
telemetry, while reserving the default `GET /status` endpoint for full on-demand storage and
topology inspection. [`rings-tui`](tui.md) renders live sparklines and latency histograms directly
from this projection.

`GET /status` returns a complete node inspection: besides peers and DHT topology, it serializes
every persistent and cache storage value (up to the configured `data_storage.capacity`, 200 MB by
default).

`GET /status?view=observability` provides a compact, bounded telemetry projection. It returns the
node version plus a bounded `observability` object whose largest collections are capped at 32
recent message records and 32 peer-rating records (under 32 KiB in production fixtures, verified by
regression test).

## Recommended collection cadence

Query `GET /status?view=observability` on a 60-second scrape interval with a single collector and
an explicit request timeout. Each request walks the live persistent DHT storage view to count
mailbox carriers, retire expired entries, and project retained peer measurements, so a 60-second
cadence keeps CPU and storage I/O negligible as node storage grows.

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
| `schema_version`, `generated_at_ms`, `process_started_at_ms`, `counter_scope` | metadata | Envelope compatibility version (`1`), assembly timestamp, process start epoch ms, and scope (`node_local_process_lifetime`). |
| `messages` | counters | Successful `sent`, `received`, `forwarded`, and `stored` operations plus aggregate `failed` count during this process. A send or forward succeeds when its next hop accepts the logical transfer. |
| `recent_messages` | bounded records | Oldest-first completion records for the last 32 message operations (`sequence`, `observed_at_ms`, `action`, `category`, `message_class`, `outcome`). `message_class` is a protocol variant, not an application-controlled topic. |
| `session_key` | gauges and counters | Active delegation `valid`, `created_at_ms`, `expires_at_ms`, `remaining_ms`, `rotation_succeeded_total`, `rotation_failed_total`, and `runtime_rotation_supported`. When `delegator_key` is configured in `config.yaml`, `rings run` renews the delegation in process before the signing horizon (`expires_at_ms - 25 s`) and increments `rotation_succeeded_total` (or `rotation_failed_total` on failure); without `delegator_key`, or after a failed renewal, the daemon exits 25 s before `expires_at_ms` so a replacement key can be loaded on restart (see [Delegations and keys](cli.md#delegations-and-keys)). |
| `mailboxes` | gauges and counter | Live `registered` relay-inbox carriers and `held_messages` retained by this node, plus `stored_total` offline-message holds during this process. |
| `dht_lookups` | counters, gauge, histogram | `total`, `succeeded`, `failed`, `timed_out` (30 s bound), and `in_flight` rounds, plus `latency_sum_ms`, `latency_count`, and 12 cumulative `latency_buckets` (`le_ms`: `5`, `10`, `25`, `50`, `100`, `250`, `500`, `1000`, `2500`, `5000`, `10000`, `30000`). |
| `peer_ratings` | bounded records | Up to 32 DID-sorted local assessments (`peer`, `reliability`, `credit_score`, `sent`, `failed_to_send`, `received`, `failed_to_receive`). Use `listPeerMeasurements` for the paginated complete set. These are advisory local assessments, not network-wide reputation. |
| `health` | gauges | `process_api_healthy` (API handler assembled dependencies), `admitted_peer_count`, `has_admitted_peer`, `has_successor`, `has_predecessor`, and `overlay_ready` (`has_admitted_peer && has_successor &&` delegation before its signing horizon). |

### Sample response

```json
{
  "version": "1.0.0-pre.8",
  "observability": {
    "schema_version": 1,
    "generated_at_ms": 1700000060000,
    "process_started_at_ms": 1700000000000,
    "counter_scope": "node_local_process_lifetime",
    "messages": { "sent": 42, "received": 38, "forwarded": 5, "stored": 1, "failed": 0 },
    "recent_messages": [],
    "session_key": {
      "valid": true,
      "created_at_ms": 1699999000000,
      "expires_at_ms": 1702591000000,
      "remaining_ms": 2591940000,
      "rotation_succeeded_total": 0,
      "rotation_failed_total": 0,
      "runtime_rotation_supported": false
    },
    "mailboxes": { "registered": 1, "held_messages": 1, "stored_total": 1 },
    "dht_lookups": {
      "total": 4, "succeeded": 4, "failed": 0, "timed_out": 0, "in_flight": 0,
      "latency_sum_ms": 120, "latency_count": 4,
      "latency_buckets": [{ "le_ms": 5, "count": 0 }, { "le_ms": 50, "count": 4 }]
    },
    "peer_ratings": [],
    "health": {
      "process_api_healthy": true,
      "admitted_peer_count": 3,
      "has_admitted_peer": true,
      "has_successor": true,
      "has_predecessor": true,
      "overlay_ready": true
    }
  }
}
```

`counter_scope` is `node_local_process_lifetime`, and `process_started_at_ms` makes resets explicit.
Use deltas or rates over cumulative counters because every process restart resets them. Mailbox
counts come from the live DHT storage view, which retires expired entries before counting them.
