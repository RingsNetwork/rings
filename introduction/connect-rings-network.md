# Connect Rings Network

Rings is a peer-to-peer network based on WebRTC transport, and as such, you have multiple ways to establish connections:

### Peer-to-Peer：&#x20;

You can establish a direct connection by exchanging Session Description Protocol (SDP). SDP is a format used to describe media session characteristics in a format that can be exchanged between peers. Read more:

[exchange-sdp.md](../advanced-topic/exchange-sdp.md "mention")

### Via Entry Point

Rings nodes are functionally equivalent, but they are divided into two categories based on the execution environment: Node nodes and Browser nodes. Node nodes are allowed to enable an external service called `EntryPoint`.

The Entry Point is an HTTP service primarily used to assist in the initialization and network connection of nodes within a firewall. As long as you can connect to one of the nodes, you can join the Rings Network.

If you have installed a Node node, you can use the following command to utilize the Entry Point:

```bash
rings connect node <entry_point url>
```

If you are using a Browser node, you can utilize the Entry Point by calling the following function:

```javascript
client.connect_peer_via_http(url)
```

Future reading:&#x20;

[handshake.md](../advanced-topic/handshake.md "mention")
