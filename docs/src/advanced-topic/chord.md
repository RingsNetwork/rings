---
description: The Chord Algorithm
---

# DHT

The Rings Network leverages the Chord algorithm for its DHT implementation. The Chord algorithm is a protocol for lookup in a peer-to-peer distributed system that allows nodes in the network to find the location of data. It enables effective routing of messages and storage of key-value pairs in a peer-to-peer setting, guaranteeing high availability in the Rings Network.

In the Rings Network, the network is organized as a ring topology, where each node is linked to two other nodes. This design maximizes the functionality of the Chord algorithm for quick and effective participant lookup and location.

## DHT

A DHT is a class of decentralized distributed systems that provide a lookup service similar to a hash table; key-value pairs are stored in a DHT, and any participating node can efficiently retrieve the value associated with a given key. The main advantage of a DHT is that nodes can join or leave the network with minimal disruption.

## Chord

Chord is a protocol and algorithm for a peer-to-peer distributed hash table. It specifies how keys are assigned to nodes, and how a node can discover the value for a given key by first locating the node responsible for that key.

Chord organizes nodes into a circular ID space, often visualized as a ring. Each node in the network has a unique identifier, and each data item identified by a key is assigned to the node whose identifier is closest to the key (according to a certain distance metric). When a node needs to find the value associated with a key, it uses the Chord protocol to locate the node responsible for that key. This is done using a "finger table" that each node maintains - a sort of routing table with pointers to other nodes in the network. This structure allows for efficient routing of queries to the appropriate node.

Chord provides a fast, distributed lookup method that scales logarithmically with the number of nodes. That is, even as the network grows large, the number of steps required to find the node storing a particular key remains relatively small. This makes it very efficient for large-scale distributed systems, where resources might be spread across many nodes and those nodes may be constantly joining or leaving the network.

## Correct Chord

Correct Chord is derived from Pamela Zave's work on Chord, and it encompasses two core principles:

* Chord must be initialized with a ring containing a minimum of `r +1` nodes, where `r` is the length of each node’s list of successors. In fact, to be proven correct, a Chord network must maintain a “stable base” of `r + 1` nodes that remain members of the network throughout its lifetime.
* The Chord Paper defined the maintenance and use of `ﬁnger tables`, which improve lookup speed by providing pointers that cross the ring like chords of a circle. **Because ﬁnger tables are an optimization and they are built from successors and predecessors, correctness does not depend on them.**

Rings Network builds upon Correct Chord and incorporates several modifications, including support for multiple successors and improved stabilization algorithms, among other enhancements.

## Finger-table convergence

Rings treats entries learned from admission, successor changes, and peer removal as routing hints,
not as proof that a finger slot is current. A lookup for slot `i` verifies the consecutive range
from `i` through the highest slot whose target is no farther than the returned successor. This
reduces sparse-table convergence from one lookup per bit to one lookup per distinct successor
range. A topology change invalidates only slots whose hints changed.

Each node keeps at most one finger lookup in flight. Reports echo a node-local request identifier;
results from an expired request or from a request invalidated by a topology change cannot overwrite
newer state. A fleet's first automatic attempt is spread over a deterministic 10-second per-node
phase window. Send cancellation, invalid reports, timeouts, and topology changes that invalidate an
in-flight proof increase a progress-sensitive retry floor through 2, 4, 8, 16, 32, and 60 seconds;
each retry is additionally spread across a full jitter window of the same size. Only an applied
range proof resets that failure level. Missed
deadlines schedule one future attempt rather than catch-up bursts, and topology stabilization and
storage repair take priority over finger convergence.

One due node emits one routed lookup, not a broadcast or recursive fan-out. Its normal completion is
one routed report; discovering an unconnected result may additionally require one routed connection
offer and answer. The per-message relay hop budget and the retry schedule therefore bound each
attempt and persistent-failure traffic separately.

These are per-node bounds: aggregate healthy bootstrap work still scales with the number of nodes.
The phase window smooths a synchronized start but is not an N-independent destination rate limit.
The destination's bounded inbound mailbox limits retained concurrent work; its origin quota remains
per origin and therefore must not be counted as a global many-origin convergence cap.
