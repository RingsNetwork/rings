# Rings Multi-Node Cluster Image

This image runs N native `rings` daemon processes in one container, generates per-node session keys from external private keys, waits for every HTTP API to become ready, then connects the nodes.

Build from the repository root:

```sh
docker build -f docker/cluster/Dockerfile -t rings-node-cluster .
```

Run a 3-node test cluster with random ephemeral private keys:

```sh
docker run --rm \
  -e RINGS_NODE_COUNT=3 \
  -e RINGS_ALLOW_RANDOM_KEYS=true \
  -p 51000-51002:51000-51002 \
  rings-node-cluster
```

Run with externally supplied keys:

```sh
docker run --rm \
  -v "$PWD/private-keys.txt:/run/secrets/rings-private-keys:ro" \
  -e RINGS_NODE_COUNT=6 \
  -e RINGS_ALLOW_RANDOM_KEYS=false \
  -p 51000-51005:51000-51005 \
  rings-node-cluster
```

Private key file format:

- one secp256k1 ECDSA private key per non-comment line
- 64-character hex or `0x`-prefixed hex
- at least `RINGS_NODE_COUNT` entries when `RINGS_ALLOW_RANDOM_KEYS=false`

The launcher does not print private key values. It writes only session key files under `RINGS_CLUSTER_DIR/keys`, which are still sensitive and should be stored on a protected volume if persisted.

Useful environment variables:

- `RINGS_NODE_COUNT`: number of nodes, for example `3`, `6`, `9`, or `18`; default `3`
- `RINGS_PRIVATE_KEYS_FILE`: key file path; default `/run/secrets/rings-private-keys`
- `RINGS_ALLOW_RANDOM_KEYS`: fill missing keys with random ephemeral keys; default `true`
- `RINGS_CONNECT_TOPOLOGY`: `ring`, `seed`, or `mesh`; default `ring`
- `RINGS_BASE_INTERNAL_PORT`: first loopback JSON-RPC port; default `50000`
- `RINGS_BASE_EXTERNAL_PORT`: first externally bound JSON-RPC port; default `51000`
- `RINGS_CLUSTER_DIR`: config, logs, storage, and session key directory; default `/var/lib/rings-cluster`
- `RINGS_ICE_SERVERS`: ICE server list passed to every node
- `RINGS_RUNTIME`: Tokio runtime flavor for each node process; default `current-thread`
- `RINGS_STABILIZE_INTERVAL`: DHT stabilization interval in seconds; default `3`
- `RINGS_DHT_FINGER_TABLE_SIZE`: optional DHT finger table slot count written into every node config

Topology modes:

- `ring`: node `i` connects to node `i+1`, and the final node connects to node `0`
- `seed`: every non-zero node connects to node `0`
- `mesh`: every pair is connected once; use carefully for larger N because handshakes grow as `N*(N-1)/2`

## 16-node DHT benchmark collection

Start a 16-node cluster with a short stabilization interval:

```sh
docker run -d --rm \
  --name rings-cluster-16-bench \
  -e RINGS_NODE_COUNT=16 \
  -e RINGS_ALLOW_RANDOM_KEYS=true \
  -e RINGS_CONNECT_TOPOLOGY=ring \
  -e RINGS_STABILIZE_INTERVAL=1 \
  -e RINGS_DHT_FINGER_TABLE_SIZE=16 \
  -e RINGS_READY_RETRIES=180 \
  -e RINGS_LOG_LEVEL=warn \
  rings-node-cluster
```

After the log shows `cluster ready`, collect status-based DHT metrics from the
real node daemons:

```sh
python3 scripts/dht_docker_cluster_bench.py \
  --container rings-cluster-16-bench \
  --nodes 16 \
  --cluster-topology ring \
  --stabilize-interval-seconds 1 \
  --docker-image rings-node-cluster \
  --samples 5 \
  --sample-interval-seconds 30
```

Add a real WebRTC transport burst sample:

```sh
python3 scripts/dht_docker_cluster_bench.py \
  --container rings-cluster-16-bench \
  --nodes 16 \
  --cluster-topology ring \
  --stabilize-interval-seconds 1 \
  --docker-image rings-node-cluster \
  --throughput-messages 1024 \
  --throughput-payload-bytes 65536 \
  --throughput-flush-timeout-ms 60000 \
  --throughput-source-index 0 \
  --throughput-destination-index 1
```

The DHT lookup metrics are computed by replaying each node's `/status`
successor and finger-table state. Reports therefore label the lookup model as
`status_snapshot_route`.

The transport section uses one internal `transportBenchmark` RPC to start a
burst inside the source node process. The reported `flush_mbps` waits for the
source node's delivery measurement to confirm that the messages have flushed to
the WebRTC send path, and `destination_received_delta` confirms how many
messages the destination accepted. This avoids the earlier per-message
`docker exec`, `curl`, HTTP JSON-RPC, and base64 overhead.
Specify `--throughput-source-index` and `--throughput-destination-index` when
you want repeatable pair selection; omit them to benchmark the first connected
in-cluster pair found in the status snapshot.
