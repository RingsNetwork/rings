<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://static.ringsnetwork.io/ringsnetwork_logo.png">
  <img alt="Rings Network" src="https://raw.githubusercontent.com/RingsNetwork/asserts/main/logo/rings_network_red.png">
</picture>


Rings RPC
===============

[![rings-node](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml/badge.svg)](https://github.com/RingsNetwork/rings-node/actions/workflows/auto-release.yml)


# JSON-RPC API endpoints

### Make requests

#### curl

Url `curl` to make the requests.

```
curl -X POST \
-H "Content-Type: application/json" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "nodeInfo", "params": []}' \
"http://127.0.0.1:50000"
```

#### javascript

TODO

## JSON-RPC methods

This section lists the rings JSON-RPC API endpoints. You can call these [APIs using a variety of tools](#Make_request).

### Error codes

The follow list contains all possible error codes and associated messages:

|code|meaning|category|
|--- |---    |---     |
|-32000|Internal service error|standard|
|-32001|Connect remote server with error occurred|standard|
|-32002|Push or find pending transport failed|standard|
|-32003|Transport not found|standard|
|-32004|Create new `transport` failed|standard|
|-32005|Close `transport` failed|standard|
|-32006|Encode data error|standard|
|-32007|Decode data error|standard|
|-32008|Register ice failed|standard|
|-32009|Create new `offer` failed|standard|
|-32010|Create new `answder` failed|standard|
|-32011|Invalid transport id|standard|
|-32012|Invalid did|standard|
|-32013|Invalid method|standard|
|-32014|Send message with error occurred|standard|
|-32015|Permission requires to do something|standard|
|-32016|VNode action error|standard|
|-32017|Register service with error occurred|standard|
|-32018|Invalid data|standard|
|-32019|Invalid message|standard|
|-32020|Invalid service|standard|
|-32021|Invalid address|standard|
|-32022|Invalid auth data|standard|


Example error response:

```json
{
    "id": 1,
    "jsonrpc": "2.0",
    "error": {
        "code": -32000,
        "message": "internal service error",
    }
}
```

### nodeInfo

return rings node basic information.

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`

#### EXAMPLE

```
curl -X POST \
-H "Content-Type: application/json" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "nodeInfo", "params": []}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `version` - current running node version

#### BODY

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
        "version": "0.0.1"
    }
}
```


### connectPeerViaHttp

Connect a peer with peer's jsonrpc endpoint

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`

#### EXAMPLE

```
## Replace REMOTE-JSONRPC-ENDPOINT with the url what you want to connect
curl -X POST \
-H "Content-Type: application/json" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "connectPeerViaHttp", "params": ["REMOTE-JSONRPC-ENDPOINT"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `transport_did` - the id of transport

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": [
        "abcd-1234"
    ]
}
```


### connectWithDid

Connect a peer with peer's did

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace REMOTE-PEER-DID with the did what you want to connect
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "connectPeerWithDid", "params": ["REMOTE-PEER-DID"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE


#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": null
}
```


### connectWithSeed

Connect a peer with peer's seed

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace REMOTE-PEER-SEED with the did what you want to connect
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "connectPeerWithSeed", "params": ["REMOTE-PEER-SEED"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE


#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": null
}
```


### createOffer

Create an offer for connection

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace REMOTE-PEER-SEED with the did what you want to connect
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "createOffer", "params": []}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `transport_id`: id of the transport
* `ice`:  ice message

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
        "transport_id": "1234",
        "ice": "abcd1234",
    }
}
```


### answerOffer

Answer an offer for connection

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace REMOTE-PEER-SEED with the did what you want to connect
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "createOffer", "params": ["REMOTE-PEER-SEED"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `transport_id`: id of the transport
* `ice`:  ice message

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
        "transport_id": "1234",
        "ice": "abcd1234",
    }
}
```


### listPeers

List all node connected peers

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "listPeers", "params": []}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `peers` - list of connected peers
  - `transport_id` - id of the transport
  - `did` - did of remote peer
  - `state` - transport state

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": [
        {
            "did": "abcd1234"
            "transport_id": "1234",
            "state": "connected",
        }
    ]
}
```


### closeConnection

Close a connected connection with the did of peer

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace REMOTE-PEER-DID with ice what you got from others
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "closeConnection", "params": ["REMOTE-PEER-DID"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {}
}
```


### listPendings

List all pending connections

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "closeConnection", "params": []}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `transport_infos` - list of all pending transports
    - `transport_id` - id of the transport
    - `state` - state of the transport

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": [
        {
            "transport_id": "abcd1234",
            "state": "new"
        }
    ]
}
```


### closePendingTransport

Close a specific pending transport

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace TRANSPORT-ID with the transport_id which in listPendings
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "closePendingTransport", "params": ["TRANSPORT-ID"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {}
}
```


### sendHttpRequestMessage

Send a http request message to remote peer, the remote peer should provide the service you want to use

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace REMOTE-PEER-DID with did of remote peer
## Replace HTTP-REQUEST-ARG with your request arguments
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "sendSimpleTextMessage", "params": ["REMOTE-PEER-DID", {HTTP-REQUEST-ARG}]}' \
"http://127.0.0.1:50000"
```

* HTTP-REQUEST-ARG
  - `name` - service name
  - `method` - http method
  - `path` - resource path
  - `timeout` - timeout of remote request, optional
  - `headers` - remote request with headers, optional
  - `body` - request body what you want to send to remote service, optional

#### RESPONSE

* `tx_id` - transaction id

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
         "tx_id": "abcd1234"
    }
}
```


### sendSimpleTextMessage

Send simple text message to a peer

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace REMOTE-PEER-DID with did of remote peer
## Replace TEXT with what you want to send
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "sendSimpleTextMessage", "params": ["REMOTE-PEER-DID", "TEXT"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `tx_id` - transaction id

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
         "tx_id": "abcd1234"
    }
}
```


### sendCustomMessage

Send custom message to a peer

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace REMOTE-PEER-DID with did of remote peer
## Replace MESSAGE-TYPE with type of your message
## Replace DATA with message payload after base64
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "sendCustomMessage", "params": ["REMOTE-PEER-DID", "MESSAGE-TYPE", "DATA"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* `tx_id` - transaction id

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
         "tx_id": "abcd1234"
    }
}
```


### publishMessageToTopic

Publish data message to specific topic

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace TOPIC with message topic
## Replace DATA with message payload after base64
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "publishMessageToTOpic", "params": ["TOPIC", "DATA"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE


#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {}
}
```


### fetchMessageToTopic

Fetch message from specific topic

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace TOPIC with message topic
## Replace DATA with message payload after base64
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "fetchMessageToTopic", "params": ["TOPIC", "INDEX"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* MESSAGES - message vec of specific topic

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": [
        "topic_message",
    ]
}
```


### registerService

Register custom service to rings network

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace NAME with the service name what you want to publish to rings network
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "registerService", "params": ["NAME"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {}
}
```


### lookupService

Lookup custom service from rings network, you can find all dids of node which provide service you want.

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace NAME with the service name what you want to lookup in rings network
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "lookupService", "params": ["NAME"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* DIDS - did list of nodes which provide service with specific name

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": [
        "did1",
        "did2",
    ]
}
```


### pollMessage

Use this method, you can pull messages received by this node, to provide your custom service,
But we suggest use `websocket` endpoint realtime get messages.

#### REQUEST

`POST http://127.0.0.1:50000`

#### HEADERS

`Content-Type: application/json`
`X-SIGNATURE: YOUR-SIGNATURE`

#### EXAMPLE

```
## Replace YOUR-SIGNATURE with your signature
## Replace WAIT with a bool value, true will block the request, until new message receive.
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "pollMessage", "params": ["WAIT"]}' \
"http://127.0.0.1:50000"
```

#### RESPONSE

* message - custom message received

#### EXAMPLE

```json
{
    "jsonrpc": "2.0",
    "id": 1,
    "result": {
      "message": {
          message_type: 1,
          data: "base64 text"
      }
    }
}
```
