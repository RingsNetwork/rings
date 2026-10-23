# Exchange SDP

We will explain how the handshake works by establishing a connection between two nodes manually.
Each node gets its own directory, delegatee key and configuration, and listens on its own pair of
JSON-RPC ports:

| Node | Internal API (`internal_api_port`) | External API (`external_api_addr`) |
|---|---|---|
| node1 | `50000` | `127.0.0.1:50001` |
| node2 | `50002` | `127.0.0.1:50003` |

The two nodes must not share any listener address: the default `external_api_addr` is
`127.0.0.1:50001`, so the second node moves both of its listeners. See the
[API](../jsonrpc.md#listeners-and-authorization) for what each listener serves.

## Prepare two nodes

Create a throwaway delegatee key for each node:

```bash
rings new-delegation -s ./node1/delegatee_key
rings new-delegation -s ./node2/delegatee_key
```

Then write the two configurations. Each lists every key `Config` requires; every key it omits takes
its default (see [config.yaml](config.yaml.md)).

```yaml
# node1/config.yaml

network_id: 1
delegatee_key: ./node1/delegatee_key
internal_api_port: 50000
external_api_addr: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50000
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 3
data_storage:
  path: ./node1/data
  capacity: 200000000
measure_storage:
  path: ./node1/measure
  capacity: 200000000
```

```yaml
# node2/config.yaml

network_id: 1
delegatee_key: ./node2/delegatee_key
internal_api_port: 50002
external_api_addr: 127.0.0.1:50003
endpoint_url: http://127.0.0.1:50002
ice_servers: stun://stun.l.google.com:19302
stabilize_interval: 3
data_storage:
  path: ./node2/data
  capacity: 200000000
measure_storage:
  path: ./node2/measure
  capacity: 200000000
```

Alternatively, `rings init --location ./node2/config.yaml --delegatee-key ./node2/delegatee_key`
replaces both steps for a node: it writes a fresh delegatee key from a random ECDSA key along with a
complete configuration, so run it instead of `rings new-delegation`, not after it. The generated
file already points `delegatee_key` at that key; only the two listeners, `endpoint_url` and the two
storage paths need editing.

## Handshake !

### Launch two nodes:

Run both from the directory that holds `node1/` and `node2/`, since the relative key and storage
paths resolve against the working directory:

```bash
rings run -c node1/config.yaml
rings run -c node2/config.yaml
```

Each node creates an owner-only `api-token` next to its configuration on first start, then prints
the token file, its DID and its internal and external JSON-RPC endpoints:

```bash
# node1
API authentication token file: node1/api-token
Did: <did1>
JSON-RPC endpoint: http://127.0.0.1:50000
JSON-RPC endpoint: http://127.0.0.1:50001

# node2
API authentication token file: node2/api-token
Did: <did2>
JSON-RPC endpoint: http://127.0.0.1:50002
JSON-RPC endpoint: http://127.0.0.1:50003
```

`createOffer` and `acceptAnswer` are internal methods and need the node's Bearer token;
`answerOffer` is public on the external listener, which is where a remote peer reaches it.

### Create Offer

Then we ask Node1 to create an offer to Node2 by:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "Authorization: Bearer $(cat node1/api-token)" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "createOffer", "params": {"did": "<did2>"}}' \
"http://127.0.0.1:50000"
```

The output is the signed offer, which carries the SDP and its ICE candidates, encoded in base58:

```bash
{"jsonrpc":"2.0","result":{"offer":"<b58 encoded offer>"},"id":1}
```

### Accept Offer and Create Answer

Then we pass the offer to Node2's external listener, which needs no token:

```bash
curl -X POST \
-H "Content-Type: application/json" \
--data '{"jsonrpc": "2.0",
        "id": 1, "method": "answerOffer",
        "params": {"offer": "<b58 encoded offer>"}}' \
"http://127.0.0.1:50003"
```

Here we will get answer responses by `Node 2`:

```bash
{"jsonrpc":"2.0","result":{"answer":"<b58 encoded answer>"},"id":1}
```

When `Node 2` already holds its own pending overlay offer to `Node 1`, the offer handed to
`answerOffer` prevails: `Node 2` releases its own and answers. A node of 0.34.2 or earlier returns
a counter-offer instead when its offer prevails on the DID tie-break (the smaller DID's offer
prevails). The encoded payload then carries a `ConnectNodeSend` rather than a `ConnectNodeReport`:
pass it to `Node 1`'s `answerOffer`, and the answer it returns to `Node 2`'s `acceptAnswer`. The
edge is then `Node 2`'s handshake, and `Node 1`'s own offer is released.

### Accept Answer

Finally, we send the answer to Node 1 to finish handshake:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "Authorization: Bearer $(cat node1/api-token)" \
--data '{"jsonrpc": "2.0",
         "id": 1, "method":
         "acceptAnswer",
         "params": {"answer": "<b58 encoded answer>"}}' \
"http://127.0.0.1:50000"
```

It will respond with an empty result, and the connection opens asynchronously:

```bash
{"jsonrpc":"2.0","result":{},"id":1}
```

`listPeers` on either node then shows the other one:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "Authorization: Bearer $(cat node1/api-token)" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "listPeers", "params": {}}' \
"http://127.0.0.1:50000"

{"jsonrpc":"2.0","result":{"peers":[{"did":"<did2>","state":"Connected"}]},"id":1}
```

The `rings` CLI performs the same three calls as `rings connect offer`, `rings connect answer` and
`rings connect accept`; see [Operate a Node from the CLI](../cli.md).

## Conclusion

So, as you can see here, the handshake process of the Rings Network can be summarized in 3 steps:

* 1\. Creating an offer and sending it to the other party;
* 2\. Accepting the offer and replying with an answer;
* 3\. Accepting the answer and completing the handshake.
