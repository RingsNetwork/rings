# Decentralized services

![A WebAssembly peer-to-peer network](../assets/webassembly-p2p-network.png)

A service on Rings has two independent parts:

1. **Discovery.** A provider registers its DID under a service name in the DHT, and a client
   looks the name up to get the providers' DIDs.
2. **Transport.** The client reaches a provider by DID through an extension protocol: a
   namespaced message, or a TCP/UDP tunnel through the built-in relay.

Registering a name in the DHT advertises the provider's DID, while extension protocols and the
declarative `services:` section in [`config.yaml`](../advanced-topic/config.yaml.md#services-and-dns-relay)
bind local sockets and handlers to serve traffic.

## Discovery

```bash
rings service register <name>
rings service lookup <name>
```

`register` appends this node's DID to the data topic `name`; `lookup` prints the topic's elements,
one per line. The topic is an ordinary [data topic](../advanced-topic/chord.md#data-topic-storage),
the one `publishMessageToTopic` and `fetchTopicMessages` also address, so a lookup returns every
text element written to it. A CLI `register` call writes a 10-minute topic entry (renewed by
re-running `register`), whereas long-lived daemons use the declarative `services:` section in
`config.yaml` or `SignedDescriptor` lease heartbeats (`ProcessorBuilder::register_task`) for
automatic, zero-tombstone renewal.

The same calls over JSON-RPC ([API](../jsonrpc.md#registerservice)):

```sh
curl -X POST \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.rings/api-token)" \
  --data '{"jsonrpc": "2.0", "id": 1, "method": "registerService", "params": {"name": "echo"}}' \
  "http://127.0.0.1:50000"

curl -X POST \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $(cat ~/.rings/api-token)" \
  --data '{"jsonrpc": "2.0", "id": 1, "method": "lookupService", "params": {"name": "echo"}}' \
  "http://127.0.0.1:50000"
```

`lookupService` returns `{"dids": [...]}`.

## Transport

### Namespaced messages

```bash
rings send message <did> <namespace> <data>
```

delivers `data` to the protocol registered under `namespace` on `did` (the `sendBackendMessage`
method). The provider handles it in an extension protocol it registered: a Rust protocol in a
node that embeds `rings-node`, or a JS handler `provider.on(namespace, initialState, handler)` in
the browser.

### Writing custom extension protocols

Every extension runs behind a namespace boundary defined by `Protocol` (a pure state machine)
and `Interpret` (an async interpreter executing effects over a namespace-scoped `Scope`):

- `Protocol::init` supplies the initial state `S`.
- `Protocol::decode(wire: Wire<'_>)` turns `(from, me, payload)` into a typed `Event` or `Reject`.
- `Protocol::step(ctx: Ctx<'_, S>, event: Event) -> Transition<S, Effect>` computes the next
  state and ordered `Vec<Effect>` synchronously and deterministically without I/O.
- Optional admission hooks (`delegates_admission`, `delegates_replay`) publish per-namespace
  policies into `rings-core` when `provider.register_protocol(protocol, shell)` installs the
  handler.
- `Interpret::interpret(&self, scope: &Scope, effect: Effect)` performs network sends
  (`scope.send(did, payload)`) or bounded self-injections confined strictly to that namespace.

In Browser JavaScript/WASM, register the identical model with
`provider.on(namespace, initialState, (ctx, event) => ({ state, effects }))`, and inspect an
example implementation in `examples/native` and `examples/dweb`.

### End-to-end (E2E) encrypted streams

After a peer handshake completes, applications can establish end-to-end ECC ElGamal encrypted
streams between DIDs without revealing plaintext to intermediate Chord relays:

1. Request the recipient's E2E public key via JSON-RPC `sendE2eHandshake` (returns `tx_id`).
2. Poll local events via `takeE2eEvents` (FFI / local RPC / `provider.take_e2e_events()`) to obtain
   the peer's public key and receive incoming decrypted stream chunks.
3. Send encrypted data via `sendE2eMessage` (`destination_did`, `recipient_public_key`, base64
   `data`), which frames and delivers ciphertext chunks to the destination's bounded E2E inbox.

See [API](../jsonrpc.md#sende2ehandshake) for the full parameter and response schema.

### TCP and UDP relay

`rings run` installs the built-in relay on the `tcp` and `udp` namespaces. You can expose local
ports declaratively via the `services:` section in
[`config.yaml`](../advanced-topic/config.yaml.md#services-and-dns-relay) (for DNS relay ingress at
`<did>.<zone>`) or register and tunnel named TCP/UDP services programmatically in Rust through
`RelayHandle`:

```rust
// Provider node: expose a local socket under a relay service name, and advertise it.
let relay = RelayHandle::install(&provider.extensions())?;
relay.register_tcp_service("echo".into(), "127.0.0.1:7000".parse()?).await?;
processor.register_service("echo").await?;

// Client node: find a provider, then tunnel a local listener to its relay service.
let relay = RelayHandle::install(&provider.extensions())?;
let request = LookupServiceRequest { name: "echo".into() };
let dids = rpc_client.lookup_service(&request).await?.dids;
let peer: Did = dids[0].parse()?;
relay.open_tcp_tunnel("127.0.0.1:9000".parse()?, peer, "echo".into()).await?;
```

A connection to the client's `127.0.0.1:9000` then reaches the provider's `127.0.0.1:7000`.
`register_udp_service` and `open_udp_tunnel` do the same for datagrams. The relay is direct:
both nodes know each other's DID, and every hop on the overlay route sees both endpoints. For
traffic that hides its endpoints, use an onion exit ([Native Gateway](../native-gateway.md)).

## Examples

[Decentralized Service Examples](../examples/decentralized-service-examples.md)
