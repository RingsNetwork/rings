# Connect Rings Network

Rings is a peer-to-peer network built on WebRTC transport, offering three flexible ways to establish peer connections:

### 1. Manual Out-of-Band Exchange (SDP)

Two peers can open a direct WebRTC datachannel without any intermediary by exchanging Session Description Protocol (SDP) offers and answers out of band. Read more in [Exchange SDP](../advanced-topic/exchange-sdp.md).

### 2. HTTP Seed Endpoint

Native nodes and Browser nodes are full peers on the same overlay. Native nodes can expose a public HTTP seed endpoint (`nodeDid` and `answerOffer`) so joining nodes—including browser tabs and nodes behind firewalls—can bootstrap into the network with a single request.

From a Native node CLI:

```bash
rings connect node https://node.rings.rs
```

From a Browser node (`@ringsnetwork/rings-node`):

```javascript
await provider.connect_peer_via_http("https://node.rings.rs")
```

### 3. Chord Overlay Routing

Once connected to at least one peer, a node can dial any other peer on the network by its DID (`rings connect did <did>`). The Chord overlay routes the WebRTC offer and returns the answer along the recorded signaling path so the two peers establish a direct datachannel.

Further reading: [How handshake works](../advanced-topic/handshake.md)
