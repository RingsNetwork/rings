![Illustration 1](/assets/blog/annual-update-from-rings-network/figure-1.png)

One year ago, I introduced the Rings network on the Cryptography mailing list

[[Cryptography] A new dWeb network that is REAL peer-to-peer, based on WebAssembly](https://www.metzdowd.com/pipermail/cryptography/2022-October/037958.html)

And today, on the same date of Bitcoin white paper released, I submitted a annual report of our project, Rings Network to the cryptopunk mail list:

[[Cryptography] Annual Update from Rings Network](https://www.metzdowd.com/pipermail/cryptography/2023-October/038232.html)

I repost the content here:

I’m here to show what we have done in the past year.

**First, let me reintroduce Rings**

The Rings network ([https://github.com/RingsNetwork/rings-node](https://github.com/RingsNetwork/rings-node)) is a fully decentralized peer-to-peer (P2P) network. I emphasize the word “fully” because Rings nodes can run on arbitrary devices, especially in web browsers. The code is written in Rust and can be compiled into WebAssembly. The transport layer is also browser-friendly, based on the WebRTC protocol, enabling direct connections between browsers (we have a demo illustrating how the handshake works: [https://rings.rs/rings-wasm-p2p/](https://rings.rs/rings-wasm-p2p/)).

**Secondly, let me highlight what we have achieved this year.**

1.) **Strong Rings DHT**: The Distributed Hash Table (DHT) is based on the Chord algorithm, and we have incorporated the work of Pamela Zave ([https://arxiv.org/pdf/1502.06461.pdf](https://arxiv.org/pdf/1502.06461.pdf)) to enhance its robustness.

2.) **WASM and FFI Client**: We now not only support WASM exports but also offer support for Foreign Function Interface (FFI). This means that developers can easily use our network in any environment or with any language, be it in a browser or native, with languages like Python or Haskell.

3.) **dWeb / TcpProxy**: Few month ago, we had a simple HTTP proxy implementation based on one-time string input/output. Now, we support TcpProxy, which is based on DIDs (Decentralized Identifiers) and can establish a TCP tunnel between any two nodes.

4.) **Services Store / Lookup**: While our primary focus is not on storage, the Rings Network has implemented basic storage functionality primarily for service discovery and registration. For example, a service providing an IPFS forward gateway can be registered as sha1(“IPFS_GATEWAY”).

5.) **Session and Abstract Account**: Rings’ design principle is to keep programs from interacting with private keys directly. So, we achieve authorization and abstract account by having users sign a delegated keypair. We now support users to authenticate using various elliptic curve encryption algorithms, including secp256k1, secp256r1, ed25519, and their variants.

6.) **Browser Extension**: We now have a browser extension that essentially works. You can find it at [https://github.com/RingsNetwork/rings_ext_v2](https://github.com/RingsNetwork/rings_ext_v2). It launches a node in the background and injects a “Rings” object into the window object in the browser, allowing the node to stay online continuously.

**Third, I will introduce what is the next move of the Rings network.**

In the upcoming development process, we have three plans:

1. Continuously improve the availability and user-friendliness of nodes, including providing better TCPProxy tools, dweb tools. They should be well-documented.

2. Optimize the browser extension; the current interface is not very appealing, and its functionality is not very intuitive. We also plan to provide more demos based on the extension.

3. Implement the DRanking protocol. The DRanking protocol is a new sampling protocol designed by us. It’s inspired by the ranking protocol of edonkey but operates in a distributed scenario. If you’re interested, you can take a look at our draft paper: [https://github.com/RingsNetwork/whitepaper/blob/master/pos.pdf](https://github.com/RingsNetwork/whitepaper/blob/master/pos.pdf)

The Rings Network is a project that I am deeply passionate about and committed to for the long term. It still has a long journey ahead. If anyone has any ideas about the future of the Rings Network, please feel free to share them. If you wish to become a contributor to the Rings Network, you are also welcome to join us.

Best Regards
Yours Ryan

---

Note: This article preserves the historical October 2023 Cryptography mailing-list progress report verbatim. In **Rings 1.0**, the repository consolidated at [github.com/RingsNetwork/rings](https://github.com/RingsNetwork/rings) with the live browser node console and unified Chrome MV3 extension + Onion WebView at [rings.rs/#node](https://rings.rs/#node), actor-isolated Merkle-authenticated CRDT storage and `r`-replica chain relay custody, guard-closed Sphinx/AEZ v5 onion Kleisli pipelines, stateless UDP/TCP + Onion TURN, and the unified [Rings 1.0](https://rings.rs/papers/rings/) and [DRanking 2.0](https://rings.rs/papers/dranking/) specifications.
