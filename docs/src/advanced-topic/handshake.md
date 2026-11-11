# How handshake works

Rings Network uses WebRTC as its transport layer, enabling a pure peer-to-peer (P2P) network where Native nodes and Browser nodes (via WebAssembly) participate as equal peers.

WebRTC is platform-agnostic and uses Interactive Connectivity Establishment (ICE) for NAT traversal. Peers establish direct, encrypted datachannels by exchanging signed Session Description Protocol (SDP) offers and answers.

Rings supports three complementary connection paths:

* **HTTP Seed Endpoint:** A joining node dials a Native node's public HTTP handshake endpoint (`nodeDid` and `answerOffer`) for fast one-step onboarding.
* **Chord Overlay Routing:** Once a node has joined the overlay, it can connect to any DID in the network (`ConnectNodeSend` / `ConnectNodeReport`) by routing the SDP handshake across the Chord ring.
* **Manual Out-of-Band Exchange:** Two nodes can exchange encoded SDP offers and answers directly over any out-of-band channel (`rings connect offer` / `answer` / `accept`) with zero server involvement.

To see the exact SDP message flow, read [Exchange SDP](exchange-sdp.md).
