# Exchange SDP

We will explain how the handshake works by establishing a connection between two nodes manually. Let's start two nodes using config1.yaml and config2.yaml, which will make the nodes listen on ports 50000 and 50001 respectively.

```yaml
# config1.yaml

bind: 127.0.0.1:50000
endpoint_url: http://127.0.0.1:50000
ecdsa_key: <privite key>
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 3
external_ip: null
backend: []
data_storage:
  path: /Users/foo/.rings/data2
  capacity: 200000000
measure_storage:
  path: /Users/foo/.rings/measure2
  capacity: 200000000

```

```yaml
# config2.yaml

bind: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50001
ecdsa_key: <private key>
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 3
external_ip: null
backend: []
data_storage:
  path: /Users/ryan/.rings/data1
  capacity: 200000000
measure_storage:
  path: /Users/ryan/.rings/measure1
  capacity: 200000000

```

## Handshake !

### Launch two nodes:

```bash
cargo run -- run -c config1.yaml
cargo run -- run -c config2.yaml
```

After that you will see your DID and signature, this signature is used for verify local RPC calling,. E.g.:

```bash
# node1
Did: <did1>
Signautre: <sig1>
JSON-RPC endpoint: http://127.0.0.1:50000
WebSocket endpoint: http://127.0.0.1:50000/ws

# node1
Did: <did2>
Signautre: <sig2>
JSON-RPC endpoint: http://127.0.0.1:50000
WebSocket endpoint: http://127.0.0.1:50000/ws
```

### Create Offer

Then we ask Node1 to create an offer by:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: <sig1>" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "createOffer", "params": []}' \
"http://127.0.0.1:50000"
```

The output is a complex JSON, which including SDP info and candidates encoded in base58:

```bash
{"jsonrpc":"2.0","result":<b58 encoded offer>,"id":1}%
```

### Accept Offer and Create Answer

Then we ask Node2 to accept the answer:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: <sig2>" \
--data '{"jsonrpc": "2.0", 
        "id": 1, "method": "answerOffer", 
        "params": ["<b58 encoded offer>"]}' \
"http://127.0.0.1:50001"
```

Here we will get answer responses by `Node 2`:

```bash
{"jsonrpc":"2.0","result":"<b58 encoded answer>","id":1}
```

### Accept Answer

Finally, we send the answer to Node 1 to finish handshake:

```bash
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: <sig1>" \
--data '{"jsonrpc": "2.0", 
         "id": 1, "method": 
         "acceptAnswer", 
         "params": ["<b58 encoded answer>"]}' \
"http://127.0.0.1:50000"
```

It will respond:

```bash
{"jsonrpc":"2.0","result":{
    "did":"<did2>","state":"checking",
    "transport_id":"<uuid of transport>"
},"id":1}%
```

## Conclusion

So, as you can see here, the handshake process of the Rings Network can be summarized in 3 steps:&#x20;

* 1\. Creating an offer and sending it to the other party;&#x20;
* 2\. Accepting the offer and replying with an answer;&#x20;
* 3\. Accepting the answer and completing the handshake.
