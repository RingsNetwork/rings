# How handshake works

Rings Network has chosen to use WebRTC as the transport layer, which facilitates the implementation of a purely peer-to-peer (P2P) network. This means that with the help of the WebRTC protocol and WebAssembly (Wasm), Rings nodes can run fully functional within web browsers.

The WebRTC protocol is platform-agnostic and leverages Interactive Connectivity Establishment (ICE) for network traversal. It enables peers to establish direct connections by exchanging Session Description Protocol (SDP) for the purpose of handshake and negotiation.

Rings Network supports two ways for establishing connections: Relay and Direct.

* Relay: This involves establishing connections through a third-party node, such as a Rings node that has an endpoint service enabled. Using a relay offers convenience in establishing connections.
* Direct: This refers to establishing peer-to-peer connections by directly exchanging SDP information. Direct connections are more decentralized in nature.

Both approaches have their advantages. Relay connections are more convenient, while direct connections provide a more decentralized approach.

To understand more about how handshake works, you may like to read:

[exchange-sdp.md](exchange-sdp.md "mention")
