# Operate a Node from the CLI

`rings` is one binary in two roles. `rings run` is the node: it joins the overlay and serves the
JSON-RPC API. Every other subcommand is a client of a running node: it reads the same
`config.yaml` to find the node's endpoint and the Bearer token that guards its internal API,
issues one request, and prints the reply. The [API chapter](jsonrpc.md) documents the methods
the clients call; this chapter documents the commands.

```text
rings <command> [options]

  init          Initializes a node with the given configuration.
  new-delegation   Creates a new delegatee signing key.
  run           Runs a foreground, composable Rings node.
  connect       Connects to a remote peer.
  peer          Manages peers on the network.
  send          Sends a message to another peer.
  service       Registers or looks up a service on the network.
  pubsub        Provides chat room-like functionality on the Rings Network.
  inspect       Show information of swarm: transport table, successors, predecessor, finger table.
```

`rings <command> --help` lists every option of a command; `rings run --help` is reproduced in
[Host a Native Node](host-a-native-node.md).

## Addressing the node

The client commands share these options:

| Option | Meaning | Default |
|---|---|---|
| `-c, --config <FILE>` | The node configuration to read | `~/.rings/config.yaml` |
| `-u, --endpoint-url <URL>` | The node's internal JSON-RPC endpoint | `endpoint_url` from the config, `http://127.0.0.1:50000` |
| `--api-token-path <FILE>` | Bearer token file of the node's internal API; a relative path resolves next to the config file | `api_token_path` from the config |

Each option is also read from the environment variable of the same name in upper case
(`ENDPOINT_URL`, `API_TOKEN_PATH`, ...).

## Join the overlay

A node needs one peer to start from; Chord stabilization discovers the rest. There are four
ways to get that first connection.

### Through a peer's HTTP endpoint

```bash
rings connect node https://node.rings.rs
```

The remote peer's external API answers the handshake (`nodeDid`, `answerOffer`) without
authentication, so no token is needed by default. A peer that gates its handshake behind its
token takes `--remote-api-token-file <FILE>`.

### Through a seed

```bash
rings connect seed file:///etc/rings/seed.json
rings connect seed https://example.org/seed.json
```

A seed is a JSON document listing peers by DID and endpoint URL; the node tries them in order:

```json
{
  "peers": [
    { "did": "0x…", "url": "https://node.rings.rs" }
  ]
}
```

An entry may carry an `api_token` for a peer whose handshake is gated.

### Through the DHT

```bash
rings connect did 0x…
```

Routes a connection request to the DID through the peers already connected; the two nodes then
exchange SDP over the overlay and open a direct datachannel.

### By hand

When neither node has a reachable HTTP endpoint, carry the handshake yourself. On the
initiator:

```bash
rings connect offer <peer-did>
```

prints an encoded offer. Deliver it to the peer, which answers it:

```bash
rings connect answer <offer>      # or `-` to read the offer from stdin
```

and prints an encoded answer. Bring that back to the initiator:

```bash
rings connect accept <answer>     # or `-`
```

[Exchange SDP](advanced-topic/exchange-sdp.md) explains what each message carries.

### Keeping a bootstrap peer reachable

Each `rings connect` form connects once. A node that later loses its transport to that peer,
and every other peer with it, stays partitioned until something reconnects it. For peers the
node should always be able to reach, list them under `bootstrap.peers` in
[config.yaml](advanced-topic/config.yaml.md#bootstrap), or hand `rings run` a seed document for
one run:

```bash
rings run --bootstrap-seed file:///etc/rings/seed.json
```

A `file://` document is read at startup; an `http(s)://` document is fetched once at startup,
so the node does not start while the document's host is unreachable.

`rings run` then supervises those targets: whenever one is no longer reachable through the
overlay it is redialed through its HTTP endpoint, in a short burst first and then every five
minutes plus up to thirty seconds of jitter, until it is admitted again. A target that other
peers can route to is left alone.

## Peers

```bash
rings peer list
rings peer disconnect <address>
```

## Messages

```bash
rings send message <to-did> <namespace> <data>
```

Delivers `data` to the protocol the peer registered under `namespace`. Receiving applications
embed `rings-node` and install a namespaced extension protocol; the removed `/ws` endpoint is not
an application-message boundary.

```bash
rings pubsub <topic>
```

Opens a chat-room loop on `topic`: each line you type is published to the topic, and messages
other peers publish to it are printed as they arrive.

## Services

```bash
rings service register <name>
rings service lookup <name>
```

`register` publishes this node as a provider of `name` in the DHT; `lookup` returns the DIDs
currently providing it. [Decentralized services](features/decentralized-services.md) describes
the model.

## Inspect

```bash
rings inspect
```

Prints the swarm as the node sees it: the transport table, successors, predecessor, and finger
table.

## Delegations and keys

`rings init` writes the configuration and a delegatee signing key (`~/.rings/delegatee_key` unless
`-s, --delegatee-key` says otherwise). `rings new-delegation` writes a fresh delegatee key. Those two
delegation-creation commands accept `-k` or `--key-file` for the account signer; `rings run` and the
RPC client commands do not. A native node stores the resulting delegatee key in plain
text, so never derive it from a key that holds assets.
