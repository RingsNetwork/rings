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
range. A topology change invalidates only slots whose hints changed. A node with no admitted
successor keeps its ranges unverified but dormant: temporary isolation is not proof that the global
membership set is empty, and the first successor admission activates the pending pass. The current
successor's authenticated stabilization report proves the local successor interval only when it
echoes the current stabilization UUID and reports the local node as its predecessor; a superseded
or reordered report is ignored. Successor-list synchronization likewise accepts only one report
whose reporter and UUID match an outstanding query to a current successor; unsolicited, duplicate,
superseded, and post-churn reports are dropped before they can start connection admission. Both paths
revalidate that claim before each candidate, so successor churn during a slow handshake stops the
remaining effects from the old report. One candidate permit already issued before churn may still
finish its connection and admission; no later candidate from that report is permitted. Candidate
lists are deduplicated: successor-list sync admits at most the local successor capacity, while
stabilization may additionally admit one predecessor. Those locally proven finger slots never send a
lookup around the ring.

Each node keeps at most one finger operation outstanding, either awaiting a lookup report or retaining
a timely proof while its returned peer completes transport admission. Reports echo a fresh 128-bit
UUID request identifier allocated at the effect boundary. Lookup expiry is checked and the proof is
retained before connection setup is awaited, so the shorter lookup deadline cannot expire during the
longer handshake. The retained proof has its own 180-second admission lease; if the handler is
cancelled or transport admission never completes, expiry releases the slot and increases the retry
backoff. Results from an expired request or from a request invalidated by a topology change
cannot overwrite newer state, including after a process restart. A fleet's first automatic attempt is spread over a
node-lifecycle-randomized 10-second per-node phase window, so an identity cannot preselect its time
bucket. Repeated `stop`/`listen` cycles on the same browser provider reuse that phase instead of
rerolling it. When browser suspension leaves a runnable finger deadline at least 10 seconds overdue,
the node spreads the stale attempt over a new 1--11 second delay rather than emitting on resume; a
retained admission proof stays dormant until that lease expires, and a full provider reconstruction
is a new lifecycle and selects new entropy. Listener generations on one browser provider are serialized,
so a new `listen` waits for a stopped generation's cooperative cleanup instead of running duplicate
maintenance daemons. An in-flight lookup exposes its remaining timeout to the listener instead of an
absolute timestamp from another clock origin, so repeated listener restarts still wake at the same
physical expiry.
Send or handshake cancellation, invalid reports, lookup timeouts, and admission-lease expiry increase a progress-sensitive
retry floor through 2, 4, 8, 16, 32, and 60 seconds. A topology change invalidates superseded evidence
without classifying normal churn as a network failure;
each retry is additionally spread across a full jitter window of the same size. Only an applied
range proof resets that failure level. Other missed
deadlines schedule one future attempt rather than catch-up bursts. Finger convergence may yield to
at most two due topology/storage phases before its turn is reserved.

One due node emits one routed lookup rather than a broadcast. Its normal completion is one routed
report. Discovering an unconnected result also runs connection admission, which sends a connect
lookup and may query the new successor's bounded successor list; those peers can in turn require
connections. The sync-storm gate therefore executes the real effect and transport path and counts
the complete causal submission trace instead of multiplying a fixed message-leg estimate. The
per-message relay hop budget bounds each carrier, while the retry schedule bounds only the originating
node's finger emissions.

These are per-node bounds: aggregate healthy bootstrap work still scales with the number of nodes.
The phase window smooths a synchronized start but is not an N-independent destination rate limit.
The destination's bounded inbound mailbox limits retained concurrent work; its origin quota remains
per origin and therefore must not be counted as a global many-origin convergence cap.
