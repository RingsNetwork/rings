# Rings Multi-Node Cluster Image

This image runs N native `rings` daemon processes in one container, generates per-node session keys from external private keys, waits for every HTTP API to become ready, then connects the nodes. It is a demo/test cluster, so onion relay and onion exit advertisement are enabled by default.

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
- `RINGS_ICE_SERVERS`: ICE server list passed to every node; set it to an empty string to run without ICE servers
- `RINGS_RUNTIME`: Tokio runtime flavor for each node process; default `current-thread`
- `RINGS_ADVERTISE_ONION_RELAY`: publish relay capability from every node; default `true`
- `RINGS_ADVERTISE_ONION_EXIT`: publish exit descriptors from every node; default `true`
- `RINGS_ONION_EXIT_SERVICES`: comma-separated `name:transport` services; default `tcp:tcp,https:tcp`
- `RINGS_ONION_EXIT_ALLOW_TARGETS`: comma-separated exit target allow-list; use `*:*` for all targets; default `*:*`
- `RINGS_ONION_EXIT_DENY_TARGETS`: comma-separated exit target deny-list; default empty

The reserved `https` onion service is TCP-backed. Native nodes advertising the default TCP exit services publish both `tcp:tcp` and `https:tcp`, which lets WorkBench build HTTPS onion proxy routes against the Docker cluster.

Topology modes:

- `ring`: node `i` connects to node `i+1`, and the final node connects to node `0`
- `seed`: every non-zero node connects to node `0`
- `mesh`: every pair is connected once; use carefully for larger N because handshakes grow as `N*(N-1)/2`
