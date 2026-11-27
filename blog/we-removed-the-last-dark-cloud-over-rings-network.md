![We Removed the Last Dark Cloud Over Rings Network](/assets/blog/we-removed-the-last-dark-cloud-over-rings-network/cover.png)

For a long time, there was a dark cloud hanging over Rings Network: when a user is on a symmetric network, a mobile network, or another environment where a peer-to-peer tunnel cannot be punched through at all, how do we achieve the decentralization we promised? The answer WebRTC gives is to relay traffic through a TURN network, yet doing that would reduce Rings Network from a pure P2P network into an ordinary relay network, making it equivalent to popular networks like Nostr.

The Rings TURN Network we implemented recently was built to confront this elephant in the room head-on. Across our recent releases, we did two things: first, we built a TURN server based on modern cryptography; second, we built a split-trust TURN relay network that preserves Tor-grade multi-hop anonymity even across strict NATs.

Here is how it works.

## 1. A TURN Server Based on Modern Cryptography

When two devices on the Internet sit behind strict NATs or mobile carrier firewalls, neither side can open a direct incoming UDP port for the other. In WebRTC, a TURN server solves this by offering a public address in the middle that receives packets from one peer and forwards them to the other.

Traditional TURN servers were designed as centralized infrastructure. To prevent abuse, a traditional TURN server maintains a user database and checks static usernames and passwords. If Rings relied on that model, someone would have to run central TURN servers, manage accounts, and distribute static credentials to every client.

In the new `rings-turn` implementation, we removed user accounts and password tables from the TURN server altogether and replaced them with modern cryptography. A node does not log in with a username and password. Instead, access is granted through short-lived cryptographic signatures and encrypted tickets that the server can verify directly in memory without consulting any account database.

Because the server is completely stateless with respect to user accounts, any node with a public address in the Rings network can run a TURN service and register it directly on the Rings DHT ring. Nodes that need a relay do not read from a hardcoded company server list; they look up available TURN services from the DHT ring itself.

## 2. A Split-Trust TURN Relay Network with Tor-Grade Endpoint Separation

Replacing accounts with cryptography and discovering TURN nodes through the DHT solves the problem of who runs the servers, but by itself it still does not solve the topology problem.

If a user behind a mobile network connects directly to a TURN node that relays UDP packets straight to the remote peer from the same server, that single TURN node sits directly between the two communicating parties. It observes the sender's IP address and the recipient's IP address together. Even if that TURN node is run by a community participant rather than a company, a single hop still holds both sides of the transport link.

![From Central TURN Servers to Multi-Hop Onion TURN](/assets/blog/we-removed-the-last-dark-cloud-over-rings-network/onion-turn.png)

To eliminate single-server linkage while remaining 100% compatible with unmodified browser and native `RTCPeerConnection` stacks, Rings combines stateless TURN with its Sphinx onion network.

First, when a node needs a relay allocation from a TURN service it found on the DHT, it does not reveal its identity to that TURN node. It requests a short-lived, single-use `SessionTicket` (`tkt-…`) through a multi-hop onion `invoke` loop on the ring (`turn.ticket`). The TURN node verifies the cryptographic ticket in memory without ever learning which `Did` requested it.

Second, in multi-hop mode (`multi_hop_onion_udp`), the entry `rings-turn-service` node never sends UDP packets directly to the remote peer from its own IP address. Instead, it bridges the allocation across a 5-hop guard-closed `onion::udp` circuit on the Rings ring to an independent `udp` onion exit node, returning that exit's bound UDP address (`XOR-RELAYED-ADDRESS`) to the WebRTC client.

Just like in the Tor network, knowledge of the connection is deliberately divided among mutually untrusted participants along the path. The entry TURN node sees the client's IP address and unlinkable `tkt-…` ticket—never the client's `Did`—and hands fixed-width AEZ-encrypted Sphinx cells into the ring without directly contacting the remote peer. Intermediate `relay` nodes peel one layer at a time and learn only their immediate neighbors. The `onion::udp` exit node at the far end enforces stateful peer-IP pinholes and forwards the WebRTC datagrams to the remote peer, seeing `(E_exit, IP_peer)` while never learning the client's real IP address or `Did`. Meanwhile, the inner WebRTC DTLS/SCTP channel remains end-to-end encrypted and DID-authenticated between the two communicating peers, so none of the intermediate relays, entry TURN, or exit node can read the contents of the traffic.

## 3. Removing the Last Dark Cloud

This design transforms how Rings behaves on the most restrictive networks on Earth.

When two nodes can punch a direct tunnel, Rings connects them directly peer-to-peer with no relay in between. When peers sit behind non-transitive NAT cliques, passive 1-hop overlay gap bridging routes across the bridge node with zero extra control frames. And when a user is on a mobile carrier network, behind a Symmetric NAT, or inside a UDP-blocked firewall, Rings seamlessly traverses via UDP/TCP cryptographic TURN and multi-hop `onion::udp` circuits—preserving sovereign peer-to-peer encryption and Tor-grade endpoint separation where no single relay observes identity and both endpoints together.

With that, the last dark cloud hanging over Rings Network is gone.

## 4. BTW: Try Rings

If you want to try Rings today, you can install or upgrade the native node from [**rings.rs**](https://rings.rs/) in one line:

```sh
curl -fsSL https://rings.rs/install.sh | sh
```

Or install the node and the terminal dashboard directly from crates.io:

```sh
cargo +1.97.0 install --locked rings-node rings-tui
```

## Further reading

- [TURN & Onion TURN Server Guide](https://rings.rs/docs/turn-server.html) — running `rings-turn` and `rings-turn-service` in single-hop and multi-hop `onion::udp` modes
- [Onion Routing & Kleisli Pipelines](https://rings.rs/docs/onion.html) — Sphinx/AEZ circuits, entry guards, SURBs, and composable `invoke` shapes
- [Interactive Terminal UI (`rings-tui`)](https://rings.rs/docs/tui.html) — live monitoring, directory inspection, and command workbench
- [Install a Native Node](https://rings.rs/docs/install-a-native-node.html) — quickstart binaries and service units

github: [https://github.com/RingsNetwork/rings](https://github.com/RingsNetwork/rings)
