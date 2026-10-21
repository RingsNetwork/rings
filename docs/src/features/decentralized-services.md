# Decentralized services

![A WebAssembly peer-to-peer network](../assets/webassembly-p2p-network.png)

A service on Rings has two independent parts:

1. **Discovery.** A provider registers its DID under a service name in the DHT, and a client
   looks the name up to get the providers' DIDs.
2. **Transport.** The client reaches a provider by DID through an extension protocol: a
   namespaced message, or a TCP/UDP tunnel through the built-in relay.

Registering a name binds no endpoint: the DHT entry holds DIDs only, and what a provider serves
is whatever its extension protocols do. There is no service backend in `config.yaml`; the node
configuration rejects unknown keys, so an old `backend:` section fails to load.

## Discovery

```bash
rings service register <name>
rings service lookup <name>
```

`register` appends this node's DID to the data topic `name`; `lookup` prints the topic's elements,
one per line. The topic is an ordinary [data topic](../advanced-topic/chord.md#data-topic-storage),
the one `publishMessageToTopic` and `fetchTopicMessages` also address, so a lookup returns every
text element written to it. A registration is a plain write and expires after 10 minutes: a
provider runs `register` again within that period to stay listed.

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

### TCP and UDP relay

`rings run` installs the relay on the namespaces `tcp` and `udp`, but registers no relay service:
the relay maps a service name to a local socket address only through `RelayHandle` in an
embedding Rust program, and neither the CLI, the JSON-RPC API nor the configuration can add one.

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
