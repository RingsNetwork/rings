![Illustration 1](/assets/blog/from-onion-routing-to-onion-op-the-evolution-of-privacy-in-rings/figure-1.png)

Rings is a structured P2P network built around data sovereignty. It allows browsers, native applications, and other types of devices to connect directly with each other, forming a ring-shaped network that provides data storage, retrieval, and services offered by participating nodes.

As these capabilities grow, the privacy challenges facing Rings are also changing. The focus is expanding from hiding who is communicating with whom to hiding how data is processed, and ultimately to allowing multiple mutually untrusted participants to complete a task together while each sees only a small part of the whole process.

This is why Rings is evolving from onion routing toward Onion OP.

## 0. What Is Onion Routing?

Onion routing was not invented by Rings, and its best-known implementation is Tor. The basic idea is simple: instead of sending data directly to its destination, the sender first selects several intermediate participants and sends the data through them one by one, preventing any single intermediate participant from knowing both where the communication started and where it will ultimately end.

The name “onion” comes from the way the data is protected with multiple layers before it is sent. Each layer is intended for a different participant along the path. The first participant can remove only the outermost layer and learn where to send the data next, while the second participant receives what remains and removes another layer. This continues until the data finally reaches its destination, much like peeling an onion layer by layer.

The important property of this design is not that every participant knows nothing, but that knowledge is deliberately divided among them. A participant close to the user may know where the data came from but not its final destination, while one close to the destination may know where the data is going but not who originally sent it. Intermediate participants generally know only their immediate neighbors, making it difficult for any single participant to reconstruct the complete communication.

Tor applies this idea to Internet access. Rings follows the same basic principle, but it does not need to build a separate network dedicated to anonymous communication. Rings already consists of interconnected nodes arranged into a ring, so the same nodes that provide storage, retrieval, and other services can also serve as intermediate participants.

## 1. Onion Routing

Rings introduced onion routing in version 0.17. Its basic approach is to select three or more nodes from the ring and send data through them sequentially before it reaches its destination, avoiding a direct connection between the two communicating parties that could easily be observed and correlated.

Before the data is sent, Rings protects it with multiple layers, much like wrapping an onion. Each node along the path can remove only its own layer, learning which neighboring node the data came from and where it should be sent next, without seeing the complete route. Even if one of these nodes is observing the communication, it sees only a small fragment of the overall process and cannot determine the original sender and final recipient from that information alone.

Because these intermediate nodes can be selected directly from the existing Rings network, the system does not depend on a dedicated set of privacy servers. As long as enough independent nodes are online, different paths can be continuously assembled from existing participants, hiding the relationship between the communicating parties among these changing routes.

However, this primarily hides the path taken by the data rather than the way the data itself is processed. No matter how many intermediate nodes the data passes through, the final service may still receive the complete input and perform the entire operation by itself. Hiding a communication relationship is therefore not the same as hiding the operation being performed.

## 2. Onion OP

The upcoming Rings 0.32 release is implementing a new mechanism built on top of onion routing, which we call Onion OP. The main difference is that nodes along the path are no longer limited to receiving and forwarding data; they can also perform a predefined operation on the data they receive and then pass the result to the next node.

For example, a task may consist of a sequence of operations such as querying, filtering, transforming, and verifying data. In a conventional service model, the data would usually be sent to a single service that performs the entire process. With Onion OP, these operations can instead be distributed across different nodes, with each node processing only what it receives before passing the result onward.

Each participant therefore needs to know only the information required to perform its own operation and where the result should go next. It does not need to know who initiated the task, which nodes came before it, what operations other nodes are performing, where it sits within the complete process, or how its output will later be used. A task that would normally be fully visible to a single service can instead be distributed among multiple independent participants.

The new design also allows the process to eventually return to its original initiator rather than sending the result backward through the same path. A complete operation therefore forms a loop through the network, while each individual participant sees only the small part of that loop directly relevant to itself.

If traditional onion routing can be compared to sending a letter through several people without allowing any intermediary to know the complete delivery route, Onion OP can be compared to dividing a job among several people who do not know each other, with each person completing only one part of the job without seeing the entire process.

## 3. More Possibilities

The significance of Onion OP goes beyond hiding network communication. Once a task can be divided into a sequence of operations, each operation can be performed by a different node, making it possible to hide more of the complete data-processing flow.

One interesting example is Onion Jev. Jev is an AI model designed specifically to make decisions rather than generate long-form content. Given some input and a set of possible choices, it selects a decision, which means a complex AI workflow can potentially be divided into a sequence of Jev decisions and distributed across different nodes through Onion OP.

In such an “Onion Jev” system, each Jev node would need to see only the information required for its current decision, without knowing the complete task, the decisions made by other nodes, or the final result. This could allow Rings Network to serve as a privacy layer supporting LLM decision-making workflows.

The same idea is not limited to Jev. Different AI models, conventional programs, and data services could all become individual steps within Onion OP, with different nodes performing decisions, queries, verification, transformation, or other operations, allowing complex tasks to be completed collaboratively without requiring any single participant to observe the entire process.

## 4. DRanking

Onion OP may also become important for DRanking, the consensus system planned for Rings 2.0. DRanking is being designed to derive trust weight from services actually provided by nodes and confirmed by other participants, creating a shared decision-making system that does not simply give equal influence to every identity in the network.

With DRanking, Rings will contain more than data transmission and storage. It will also produce service records, trust weights, and results collectively confirmed by the network, which introduces a new privacy question: how can these shared results be established without unnecessarily exposing the behavior of individual participants?

Onion OP provides one possible foundation for addressing this problem by allowing some future operations related to DRanking to be performed across multiple nodes rather than inherently requiring a single participant to observe the complete process.

In this sense, Onion OP is not only an extension of Rings’ current anonymous communication capabilities; it also leaves room for Rings 2.0 to explore a deeper combination of privacy and decentralized consensus.

## 5. BTW: Try Rings

If you’ve made it this far and want to experience what Rings can already do today, rather than just read about where it is heading, you can download the latest native Rings node from **rings.rs**, or use the WASM node built directly from the latest master branch. The WASM version already provides several basic features, including Onion Browser and direct communication between connected nodes.

Keep in mind that Rings is still **pre-1.0**. Until the official 1.0 release, we make no forward-compatibility guarantees. Protocols, APIs, and data formats may change as development continues, so current releases are best suited for experimentation, testing, and contributing to development rather than assuming compatibility with future versions.

If you find a bug or have a new idea for Rings, feel free to open an Issue directly on GitHub.

github: [https://github.com/RingsNetwork/rings](https://github.com/RingsNetwork/rings)
