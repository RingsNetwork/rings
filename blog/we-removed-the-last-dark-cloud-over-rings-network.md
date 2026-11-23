![We Removed the Last Dark Cloud Over Rings Network](/assets/blog/we-removed-the-last-dark-cloud-over-rings-network/cover.png)

For a long time, there was a dark cloud hanging over Rings Network: when a user is on a symmetric network, a mobile network, or another environment where a peer-to-peer tunnel cannot be punched through at all, how do we achieve the decentralization we promised? The answer WebRTC gives is to relay traffic through a TURN network, yet doing that would reduce Rings Network from a pure P2P network into an ordinary relay network, making it equivalent to popular networks like Nostr.

The Rings TURN Network we implemented recently was built to confront this elephant in the room head-on. Across our last two releases, we did a couple of things: first, we built a TURN server based on modern cryptography; second, we built a TURN relay network whose worst-case outcome is equivalent to the Tor network.

Here is how it works.

## 1. A TURN Server Based on Modern Cryptography

When two devices on the Internet sit behind strict NATs or mobile carrier firewalls, neither side can open a direct incoming UDP port for the other. In WebRTC, a TURN server solves this by offering a public address in the middle that receives packets from one peer and forwards them to the other.

Traditional TURN servers were designed as centralized infrastructure. To prevent abuse, a traditional TURN server maintains a user database and checks static usernames and passwords. If Rings relied on that model, someone would have to run central TURN servers, manage accounts, and distribute static credentials to every client.

In the new `rings-turn` implementation, we removed user accounts and password tables from the TURN server altogether and replaced them with modern cryptography. A node does not log in with a username and password. Instead, access is granted through short-lived cryptographic signatures and encrypted tickets that the server can verify directly in memory without consulting any account database.

Because the server is completely stateless with respect to user accounts, any node with a public address in the Rings network can run a TURN service and register it directly on the Rings DHT ring. Nodes that need a relay do not read from a hardcoded company server list; they look up available TURN services from the DHT ring itself.

## 2. A TURN Relay Network Whose Worst Case Is Equivalent to Tor

Replacing accounts with cryptography and discovering TURN nodes through the DHT solves the problem of who runs the servers, but by itself it still does not solve the topology problem.

If a user behind a mobile network connects directly to a TURN node to relay traffic to another peer, that TURN node sits directly between the two communicating parties. It knows the sender's real IP address, and it knows the recipient's IP address. Even if that TURN node is run by a community participant rather than a company, the connection has still degenerated into a single-hop relay, which is structurally no different from a Nostr relay.

![From Central TURN Servers to Multi-Hop Onion TURN](/assets/blog/we-removed-the-last-dark-cloud-over-rings-network/onion-turn.png)

To prevent that degradation, Rings combines TURN with the onion routing network that already runs on the ring.

First, when a node needs a relay allocation from a TURN node it found on the DHT, it does not contact that TURN node directly. It requests a short-lived relay ticket through a multi-hop onion loop on the ring. The TURN node can verify that the request and the ticket are valid, but because the request arrived through an onion loop, the TURN node never learns which identity or IP address asked for it.

Second, when the node begins sending UDP traffic through the TURN allocation, it does not send those UDP packets directly from its own IP address to the TURN node either. Instead, it wraps the UDP traffic in multiple layers of onion encryption and routes it through several intermediate nodes on the Rings ring before the traffic finally exits to the TURN node.

Just like in the Tor network, knowledge of the connection is deliberately divided among different participants along the path. The first node on the path knows the user's real IP address, because the user is connected to it directly, but it can remove only the outermost encryption layer and knows only which intermediate node to pass the data to next. It does not know that the traffic is headed to a TURN server, nor who the final peer is. The TURN server at the end of the onion path forwards the WebRTC traffic to the remote peer, but it sees only the last onion node and never learns the user's real IP address. Meanwhile, the inner WebRTC channel remains end-to-end encrypted between the two communicating peers, so none of the intermediate nodes or the TURN server can read the contents of the traffic.

## 3. Removing the Last Dark Cloud

This design changes what happens to Rings when the underlying physical network is at its worst.

When two nodes can punch a direct tunnel, Rings connects them directly peer-to-peer with no relay in between. When a user is on a mobile network or behind a symmetric NAT where direct hole-punching is impossible, Rings no longer has to choose between failing to connect and degenerating into a Nostr-style relay network. Even when traffic must pass through a TURN relay, the connection degrades at worst into a multi-hop anonymous circuit equivalent to the Tor network, where no single participant can observe both ends of the communication.

With that, the last dark cloud hanging over Rings Network is gone.

## 4. BTW: Try Rings

If you want to try Rings `v1.0.0-pre.8` today, you can install or upgrade the native node from **rings.rs** in one line:

```sh
curl -fsSL https://rings.rs/install.sh | sh -s -- --version v1.0.0-pre.8
```

Or install the node and the terminal dashboard directly from crates.io:

```sh
cargo install rings-node rings-tui --version 1.0.0-pre.8
```

github: [https://github.com/RingsNetwork/rings](https://github.com/RingsNetwork/rings)
