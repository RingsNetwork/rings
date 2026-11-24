# API

A native node serves JSON-RPC 2.0 over HTTP `POST /` on two listeners. This page is the one
reference for its methods; the method set is `rings_rpc::method::Method`, and every params and
result object below is the matching `Request`/`Response` struct of `rings_rpc::protos::rings_node`.

## Making requests

```sh
curl -X POST \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.rings/api-token)" \
  --data '{"jsonrpc": "2.0", "id": 1, "method": "nodeInfo", "params": {}}' \
  "http://127.0.0.1:50000"
```

* Every request must use `Content-Type: application/json`.
* `params` is one JSON object whose fields are those listed for the method; a method without
  parameters takes `{}`.
* Gated methods require the listener's credential as `Authorization: Bearer <token>`.
  `rings init` writes the token to an owner-only `api-token` file next to the YAML
  configuration, and the `rings` CLI reads it automatically.

### Listeners and authorization

| Listener | Methods | Authorization |
|---|---|---|
| internal (`internal_api_port`, default 50000) | every method | Bearer required |
| external (`external_api_addr`, default 127.0.0.1:50001) | `nodeDid`, `answerOffer`, `acceptAnswer` | public |
| external (`external_api_addr`, default 127.0.0.1:50001) | `nodeInfo`, `lookupOnlineNodes`, `lookupOnionExits` | Bearer required |

The external listener refuses every other method. The public methods cover peer discovery and
both remote halves of an out-of-band handshake: `nodeDid` reveals the DID the caller is about to
address, `answerOffer` admits a peer-signed offer, and `acceptAnswer` completes an initiator's
pending offer. A batch is authorized by its strictest member, so a batch mixing a public and a
gated method requires the token. Batches are bounded: at most `8` unauthenticated public calls per
batch (`MAX_PUBLIC_BATCH_CALLS`, larger public batches require authentication) and at most `64`
total calls per batch (`MAX_JSONRPC_BATCH_CALLS`, larger batches fail with HTTP 400). The
[node README](https://github.com/RingsNetwork/rings/tree/master/crates/node#control-api-security)
covers browser origins, remote binding and TLS.

In addition to `POST /` JSON-RPC, listeners expose HTTP `GET` routes requiring the Bearer token:
`GET /status` (full `NodeInfoResponse`), `GET /status?view=observability` (compact
[operator observability](operator-observability.md) projection), and internal-only
`GET /gateway/status` (`GatewayStatus` when the [native TUN gateway](native-gateway.md) is
active). Every HTTP response carries `x-node-version: <crate_version>`.

### Errors

Errors follow JSON-RPC 2.0:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": { "code": 500, "message": "Invalid did: ..." }
}
```

* `-32700` parse error, `-32600` invalid request, `-32601` unknown method, `-32602` invalid
  params (a missing field, a malformed DID, data that is not base64, a page limit out of range),
  `-32603` internal error.
* A failure inside the node carries the stable positive code of
  `rings_node::error::Error::code()`, whose hundreds name the category: `1xx` remote RPC, `2xx`
  connection and handshake, `3xx` encoding, `4xx` Wasm, `5xx` DID, method and permission, `6xx`
  connect, send and DHT entries, `7xx` JavaScript interop, `8xx` data, storage and configuration,
  `9xx` files, `10xx` serialization, `11xx` core, `12xx` external signer / FFI, `15xx` extensions
  and runtime, `16xx` onion.
* `210` is a handshake with a node of another wire version (see [Wire version](#wire-version)).
  Its `data` names both: `{"ours": 5, "theirs": 4}`, with `theirs` null for a node from before
  wire versioning. A client matches the code and the data, never the message.

### Wire version

Every frame a node sends, and so every offer and answer of a handshake, opens with the fixed
header `"RINGS\0" ‖ wire_version` (`u16` big-endian; `WIRE_VERSION = 6` in 1.0). The wire version
goes up by one per release, relative to the last released wire version, when a release changes
anything on the wire that a node of that version would reject or misread; it is independent of
the crate version. There is no multi-version negotiation: a node accepts only its own
`WIRE_VERSION`, so two nodes of different wire versions never connect and each refuses the
other's offer or answer before decoding its body (error `210` over JSON-RPC). The `1.0.x` release
line freezes `WIRE_VERSION = 6`; any future wire-version bump is an explicit coordinated network
cutover. A client learns a node's wire version before joining it from the `wire_version` of its
public `nodeDid`, and its crate version from the `x-node-version` header of every API response.

## Methods

| Method | Params | Result |
|---|---|---|
| [`nodeDid`](#nodedid) | — | `did`, `wire_version` |
| [`nodeInfo`](#nodeinfo) | — | `version`, `swarm` |
| [`connectPeerViaHttp`](#connectpeerviahttp) | `url`, `api_token?` | `did` |
| [`connectWithDid`](#connectwithdid) | `did` | — |
| [`connectWithSeed`](#connectwithseed) | `peers` | — |
| [`listPeers`](#listpeers) | — | `peers` |
| [`createOffer`](#createoffer) | `did` | `offer` |
| [`answerOffer`](#answeroffer) | `offer` | `answer` |
| [`acceptAnswer`](#acceptanswer) | `answer` | — |
| [`disconnect`](#disconnect) | `did` | — |
| [`sendBackendMessage`](#sendbackendmessage) | `destination_did`, `namespace`, `data` | — |
| [`sendE2eHandshake`](#sende2ehandshake) | `destination_did` | `tx_id` |
| [`sendE2eMessage`](#sende2emessage) | `destination_did`, `recipient_public_key`, `data`, `max_plaintext_frame_len?` | `stream_id` |
| [`publishMessageToTopic`](#publishmessagetotopic) | `topic`, `data` | — |
| [`fetchTopicMessages`](#fetchtopicmessages) | `topic`, `skip` | `data` |
| [`registerService`](#registerservice) | `name` | — |
| [`lookupService`](#lookupservice) | `name` | `dids` |
| [`lookupOnlineNodes`](#lookuponlinenodes) | `include_expired?` | `nodes` |
| [`lookupOnionExits`](#lookuponionexits) | `service?`, `include_expired?` | `exits` |
| [`peerMeasurement`](#peermeasurement) | `did` | `measurement` |
| [`listPeerMeasurements`](#listpeermeasurements) | `limit?`, `cursor?` | `measurements`, `next_cursor` |

A field marked `?` may be omitted. A method whose result is `—` returns `{}` on success.

### nodeDid

The DID of this node. Public on the external listener.

* Params: `{}`.
* Result:
  * `did`: the node's DID as a hex string;
  * `wire_version`: the node's [wire version](#wire-version); absent from a node before wire
    versioning.

```json
{"jsonrpc": "2.0", "id": 1, "method": "nodeDid", "params": {}}
```

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"did": "0x11e807fcc88dd319270493fb2e822e388fe36ab0", "wire_version": 5}}
```

### nodeInfo

The node's version and an inspection of its swarm.

* Params: `{}`.
* Result:
  * `version`: the node version;
  * `swarm`: `null`, or
    * `peers`: as in [`listPeers`](#listpeers);
    * `dht`: `{did, successors, predecessor, finger_table_ranges: [{did, start, end}]}`;
    * `persistence_storage`, `cache_storage`: `{items: [{key, value: {did, kind, data}}]}`,
      each element of `data` the base64 of its bytes.

### connectPeerViaHttp

Connect to the peer behind a JSON-RPC endpoint by the HTTP handshake: this node calls the
remote `nodeDid`, sends it an offer with `answerOffer`, and accepts the answer. A peer whose
`nodeDid` states another wire version, or none, is refused with error `210` before any offer.

* Params:
  * `url`: the remote endpoint, normally its external listener;
  * `api_token` (optional): a Bearer token for a remote endpoint fronted by a proxy that demands
    one; absent by default, since the handshake is public.
* Result: `did`, the DID of the peer the handshake reached.

```json
{"jsonrpc": "2.0", "id": 1, "method": "connectPeerViaHttp",
 "params": {"url": "https://node.example:50001"}}
```

### connectWithDid

Connect to a peer by DID, signalling the offer and answer through the DHT. The node must already
be on the ring.

* Params: `did`.
* Result: `{}` once the offer is sent, or at once when the peer is already connected; the
  connection opens asynchronously.

### connectWithSeed

Connect to a set of seed peers by the HTTP handshake. The set is validated before any entry is
dialled, so an invalid or ambiguous document dials nothing, and a peer listed twice is dialled
once. A peer already connected or being connected is skipped.

* Params: `peers`, a list of `{did, url, api_token?}`; the handshake fails unless the endpoint
  `url` answers as `did`.
* Result: `{}` when every owed dial succeeded.

### listPeers

The peers this node has connections to.

* Params: `{}`.
* Result: `peers`, a list of `{did, state}`, where `state` is the connection state the swarm
  reports.

### createOffer

Create an offer for a manual handshake with `did`. Deliver it to the peer out of band; the peer
passes it to its `answerOffer`.

* Params: `did`, the peer that will answer.
* Result: `offer`, the encoded signed offer.

### answerOffer

Answer an encoded offer. Public on the external listener.

* Params: `offer`, as returned by `createOffer`. An offer of another wire version is refused
  with error `210`.
* Result: `answer`, the encoded answer. Any compatible offer handed to `answerOffer` prevails
  over this node's own pending overlay offer to the offering peer, which is released. On a
  simultaneous manual/out-of-band DID tie-break where this node's pending external offer prevails,
  it returns a counter-offer (`ConnectNodeSend` payload): answer it with `answerOffer` on the
  other node and return that answer via `acceptAnswer`.

### acceptAnswer

Accept an encoded answer to an offer this node created, which opens the connection. Public on
the external listener.

* Params: `answer`, as returned by the peer's `answerOffer`.
* Result: `{}`.

### disconnect

Close this node's connection to `did`.

* Params: `did`.
* Result: `{}`.

### sendBackendMessage

Send an extension message to `destination_did`, routed to the protocol `namespace` there.

* Params:
  * `destination_did`;
  * `namespace`: the extension protocol namespace;
  * `data`: the payload bytes, standard base64.
* Result: `{}`.

### sendE2eHandshake

Ask `destination_did` for its end-to-end encryption public key.

* Params: `destination_did`.
* Result: `tx_id`, the transaction id of the handshake request.

### sendE2eMessage

Send bytes to `destination_did` encrypted end to end under `recipient_public_key`.

* Params:
  * `destination_did`;
  * `recipient_public_key`: base58-check, or hex;
  * `data`: the plaintext bytes, standard base64;
  * `max_plaintext_frame_len` (optional): the plaintext frame length; `0` or absent is the core
    default.
* Result: `stream_id`, the id of the encrypted stream.

### publishMessageToTopic

Append a text element to the DHT data topic `topic`.

* Params: `topic`, `data` (UTF-8 text).
* Result: `{}`.

### fetchTopicMessages

Read the elements of the DHT data topic `topic`.

* Params: `topic`, `skip` (the number of leading elements to omit).
* Result: `data`, the remaining elements as UTF-8 text; an element that is not UTF-8 is omitted,
  and a topic not found has no elements.

### registerService

Register this node as a provider of the service `name`: append its DID to the topic `name`.

* Params: `name`.
* Result: `{}`.

### lookupService

The providers registered for the service `name`.

* Params: `name`.
* Result: `dids`, the registered DIDs.

### lookupOnlineNodes

The signed descriptors of the online-node registry.

* Params: `include_expired` (optional, default `false`).
* Result: `nodes`, a list of descriptors with `did`, `public_key`, `delegatee_public_key`,
  `node_type` (`Native`, `Browser` or `Ffi`), `network_id`, `storage_redundancy`,
  `dht_virtual_nodes`, `capabilities` (`{onion_relay, holds_relay_mail}`: the relay's process
  epoch or `null`, and whether the node keeps a relay store and so holds relay mail),
  `endpoint_hint`, `started_at_ms`, `heartbeat_at_ms`, `fresh_for_ms`, `expires_at_ms`,
  `version` and `signature`. A descriptor is a signed lease: `expires_at_ms` is the
  lease end and `fresh_for_ms` the freshness window, both signed, and `heartbeat_at_ms` is when
  the serving node last heard it (its registry element's dot), which is not signed. A descriptor
  is live while the time is at most the lease end and at most `heartbeat_at_ms + fresh_for_ms`
  plus the 3 s skew tolerance; a lease ending more than 10 minutes and 3 s ahead, or a window
  over 10 minutes, is never live.

### lookupOnionExits

The signed onion-exit descriptors.

* Params:
  * `service` (optional): `tcp`, `https`, or `udp`; empty or absent means every service;
  * `include_expired` (optional, default `false`).
* Result: `exits`, a list of descriptors with `did`, `public_key`, `delegatee_public_key`,
  `process_epoch`, `node_type`, `network_id`, `service`, `policy` (`{allowed_targets,
  denied_targets, max_sessions, max_bytes_per_minute}`), `started_at_ms`, `heartbeat_at_ms`,
  `fresh_for_ms`, `expires_at_ms`, `version` and `signature`, read as in `lookupOnlineNodes`.

### peerMeasurement

This node's local measurement of one peer.

* Params: `did`.
* Result: `measurement`, `null` when the peer was never measured, or
  * `did`;
  * `counters`: `{connected, disconnected, sent, failed_to_send, received,
    failed_to_receive}`;
  * `credit`: `null`, or `{bytes_sent_to_peer, bytes_received_from_peer, last_seen_seconds,
    score}`, with `score` in `[1, 10]`;
  * `reliability`: `healthy`, `unknown` or `degraded`.

### listPeerMeasurements

One page of this node's peer measurements, in DID order.

* Params:
  * `limit` (optional): entries per page, from 1 to 1000; absent means 100;
  * `cursor` (optional): the `next_cursor` of the previous page.
* Result: `measurements`, as in [`peerMeasurement`](#peermeasurement), and `next_cursor`,
  `null` at the end.

## Provider-local RPC methods

In addition to the 21 HTTP JSON-RPC methods above, embedded hosts using
[`rings_node_request`](ffi.md) (C FFI) or `provider.request(method, params)` (Browser WASM) can
invoke six built-in local methods handled by `Provider::request_internal`:

| Method | Params | Result | Purpose |
|---|---|---|---|
| `takeE2eEvents` | `{}` | `{ events, dropped }` | Drain up to 4096 buffered E2E handshake and decrypted stream-frame events (`dropped` counts overflow) |
| `dispatchDropCounters` | `{}` | drop counters by reason | Inspect `Backend` envelope drop counts per reason |
| `signingHorizon` | `{}` | `{ expires_at_ms, horizon_ms, remaining_ms, signs_now }` | Inspect active delegation signing horizon (`25 s` before expiry) |
| `renewDelegateeKey` | `{ delegatee_key }` | same as `signingHorizon` | Hot-swap a newly minted `DelegateeKey` in process without restarting |
| `lookupRegistry` | `{ topic }` | `{ topic, network_id, now_ms, elements: [{ data_base64, heartbeat_at_ms }] }` | Read raw normalized registry elements with CRDT dots (used by `turn_services`, `onion_invokes`, etc.) |
| `onionInvoke` | `{ shape, backend, constants_base64?, input_base64, eligible_dids?, eligible_stages?, timeout_ms? }` | `{ stage_did, stage_process_epoch, stage_delegatee_public_key, output_base64 }` | Evaluate a 1-stage Kleisli `invoke` loop over a guard-closed Sphinx circuit |

Crates embedding `Provider` can also attach domain methods via `Provider::register_local_rpc_method`
(for example `lookupTurnServices` and `requestTurnTicket` installed by `rings-turn-service`).
