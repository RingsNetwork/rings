# Start Here

<img class="rings-mark" src="assets/rings.svg" alt="" width="56" height="56">

**Rings is a peer-to-peer network for the sovereign age.** Browser tabs and native daemons join
one overlay, find each other by DID, and exchange messages over direct WebRTC datachannels
routed by a Chord DHT. There is no server in the data path: seed nodes only help a peer find
its first connection.

On top of the overlay, Rings gives applications a namespace-scoped protocol runtime. A
protocol is a pure state machine; its interpreter shell performs the side effects, and only
within its own namespace. Built-in protocols cover peer service relay and echo; yours are
registered under a namespace of their own.

Rings is written in Rust and compiles to native binaries and to WebAssembly, so the same node
runs in a terminal, in a browser tab, and inside a host that speaks C. This book is the
reference for all three; the [Guide](https://rings.rs/#guide) on the site is its short form.

## Choose a runtime

| Runtime | Start with |
|---|---|
| Native node | [Install](install-a-native-node.md), then [Host a Native Node](host-a-native-node.md) and [Operate a Node from the CLI](cli.md) |
| Browser (Wasm) node | [Build for Wasm](build-for-wasm.md) and [Browser Node and Extension](browser-node.md) |
| Another language | [Embed via C FFI](ffi.md) |
| An AI coding agent | [llms.txt](llms.md) |

If you are new to Rings, start with a native node: it is the shortest path to a running peer
and to the JSON-RPC [API](jsonrpc.md) every runtime shares.

## How it works

- [Architecture](advanced-topic/architecture.md): the layers, from transport to application.
- [How handshake works](advanced-topic/handshake.md) and [Exchange SDP](advanced-topic/exchange-sdp.md): how two peers open a datachannel.
- [DHT - Network Layer](advanced-topic/chord.md): routing and lookup on the Chord ring.
- [Account Abstraction](advanced-topic/account-abstraction.md): DIDs and the signature schemes behind them.
- [config.yaml](advanced-topic/config.yaml.md): every section of a native node's configuration.

## Beyond the book

- [Rings whitepaper](https://github.com/RingsNetwork/rings/blob/master/papers/rings.pdf): the protocol paper.
- [Security model](https://github.com/RingsNetwork/rings/blob/master/SECURITY.md): the assumptions, deployment models, and the boundary between DID authentication and Sybil resistance. Read it before deploying.
- [Roadmap](https://github.com/RingsNetwork/rings/blob/master/ROADMAP.md): where the network layer and the privacy layer are heading.
- [Source on GitHub](https://github.com/RingsNetwork/rings): the code, examples, and issues.
- [License](https://github.com/RingsNetwork/rings/blob/master/LICENSE): AGPL-3.0-only, so a product or hosted service built on Rings publishes its source; a commercial license for use outside those terms is available on request.
