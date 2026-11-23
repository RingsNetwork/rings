## *Fixing Chord finger maintenance for a ring made of browsers, and the bugs that hid behind a cooperative test harness.*

Rings is a Chord ring. Most of its members are browser tabs that talk to each other over WebRTC. That second sentence is the whole problem, and this post is about a pull request that took me longer than I expected: making the finger table converge on a network where a “connection” can take two minutes to open and often never does.

If you already know Chord, skip the next paragraph.

Chord gives every node an identifier on a ring of size 2¹⁶⁰ and a *finger table*: entry i points at the first live node at or after me + 2^i. With 160 such entries a lookup takes O(log N) hops. The original 2001 paper builds the table carefully; the 2003 journal version replaces that with a background loop that refreshes one random entry per period. Almost every implementation I've read, including the one Rings shipped before this change, inherits the 2003 loop.

## What was wrong

Three things, and they compounded.

The loop refreshed one slot per stabilization round. A 160-slot table takes on the order of 160 rounds to become trustworthy. With a 15-second period that is forty minutes, and browser nodes rarely live forty minutes.

Worse, the table could not tell a guess from a fact. When a peer joined, we wrote it into every slot it *could* own. That is a fine heuristic; the 2001 paper suggests it. But once written, a guessed entry looked identical to a verified one, so a wrong guess made from a stale view survived until its slot’s turn came up in the loop.

And lookup results were matched to slots by slot index alone. A result from before a topology change, or from before a process restart, could land on a slot that had since been rewritten and overwrite the newer value. Numeric request counters that restart at zero made the restart case easy to hit.

None of this breaks Chord’s *correctness*. Pamela Zave proved years ago that ring safety rests on the successor structure alone and fingers are an optimization. But an optimization you cannot tell from a guess is a routing table you cannot trust, and on a sparse ring you feel every wasted hop.

## The lemma that was already there

The fix starts with something the 2001 paper says in section 4.4 and the 2003 paper quietly dropped. When node n looks up slot i and gets successor s at clockwise distance d, it has learned more than one entry. By the definition of a successor, the arc from n + 2^i to s contains no member. Every later target n + 2^j with 2^j <= d lies on that arc, so it has the same successor. One lookup proves the whole run of slots i through floor(log2 d).

On a sparse ring this is dramatic. If the network has a thousand nodes, a 160-slot table holds around ten distinct values; most slots point at the immediate successor. A full pass needs one lookup per distinct value, not one per bit. And the lowest run, the slots whose targets fall before the immediate successor, needs no lookup at all: the successor’s own stabilization report, the one that says “my predecessor is you”, proves it.

I want to be precise about attribution because I got it wrong in the first draft of the design notes. This is Chord’s init_finger_table check, figure 6 of the SIGCOMM paper. Rings did not invent it. What Rings adds is everything the paper did not need because it assumed a network where "connect to s" is a function call.

## Hints and proofs

The table now stores two things per slot: the *hint* (what we think the entry is) and *evidence* (whether a lookup, or a stabilization report, has verified it since the hint last changed).

The evidence part is a small version counter. Every time a topology event changes any hints, the table bumps an epoch and stamps the changed slots with it. A lookup carries the epoch at which it was issued. When its result comes back, it may write a slot only if that slot’s stamp is not newer than the lookup’s epoch. A late result cannot roll back a newer hint; it just skips the slots that moved on.

That sounds like a lot of bookkeeping for a routing cache, and I went back and forth on it. The alternative, “never write a result that arrives after a change”, throws away good information whenever the ring is busy, which is exactly when you need it.

## One attempt, one token

A node keeps at most one finger operation outstanding. The state is an enum with three shapes:

```
enum FingerAttempt {
    Idle,
    AwaitingReport(PendingFingerLookup),   // request, issue epoch, 10 s deadline
    AwaitingAdmission(AdmissionFingerProof), // validated proof, 210 s lease
}
```

Each lookup gets a fresh UUID. A report is accepted only if it carries the exact (slot, uuid) the node is waiting for; anything else is stale and changes nothing, not even the retry counter. Restarts stop being a problem because a restarted process has no attempt to match.

The third shape is the one browsers forced on me. A lookup often returns a peer we are not connected to. Before we can route through it we need a WebRTC handshake, and that handshake is bounded at 180 seconds by the transport. The lookup’s own deadline is 10 seconds. In the first version of this PR the report arrived, the handshake started, and ten seconds later the scheduler expired the lookup, charged a failure, and discarded the proof. The one case that actually improves routing was being penalized. Now a timely report moves its proof into AwaitingAdmission *before* the handshake is awaited, with its own lease, deliberately longer than the handshake generation so that the transport's own expiry, not the lease, is what normally ends it.

## The bug I shipped first

The scheduler picked the lowest unverified slot, ran one attempt at a time, and only reopened proved ranges for periodic revalidation once every slot was verified. Read those three rules together and you get a node that can never finish.

Take one slot whose true successor is alive but cannot complete a handshake with us. That is not exotic; it is the normal NAT case in a browser mesh. Each round: lookup, report, defer, connect fails, retire, back off, pick the lowest unverified slot, which is the same slot. Every slot above it stays unverified forever. And because “all verified” never holds, the periodic revalidation of every *other* range is frozen too. The master branch’s dumb round-robin loop could not do this. My clever one could.

The fix is a cursor. Selection resumes after the last range an attempt proved or failed, wrapping around, so every unverified slot gets one attempt per rotation and the failing one costs exactly one turn. A failure also skips the whole run of slots that share the failed hint, since they expect the same successor and would fail the same way. Revalidation reopens at the same cursor, and it is deferred by pending work only while that work is *succeeding*; a failing slot no longer blocks it.

That is a two-line change in the selection function and a one-clause change in a predicate. It took a full re-read of the PR to see it, after the tests were green. The tests were green because the oracle harness answered every lookup correctly, and no test had a peer that answered and then refused to connect.

## Reports that may open connections

Two other message types can cause a node to open connections: the stabilization report from its successor, and the successor-list report it asks for on join. Before this PR both were accepted from any origin at any time. An old report, or a forged one from a peer that happened to be a successor, could make a node open a handful of handshakes.

Both are now correlated. The node records (head, uuid, Requested) when it sends the query. The report must come from that head, carry that UUID, and be the first to *claim* the token, which moves it to Processing. The candidate list is bounded before any per-candidate work, to successor capacity plus one for the predecessor, so a report of any length costs at most K+1 connection attempts. Each candidate re-checks the token before its handshake, so head churn revokes the remainder.

I broke this twice in review before it was right. First, a new stabilization round every period overwrote a claim that was still being processed; when one candidate handshake outlasted the period, the report was never applied at all. The fix is to skip the round while the previous report is Processing. That is only safe if a claim can never be left behind by a handler that was cancelled, so claims became guards that release on drop:

```
pub(crate) struct StabilizationClaim<'ring> {
    ring: &'ring PeerRing,
    request_id: uuid::Uuid,
}
```

```
impl Drop for StabilizationClaim<'_> {
    fn drop(&mut self) {
        let _ = self.ring.cancel_stabilization(self.request_id);
    }
}
```

Second, and this one cost me an afternoon: the first version built the guard with bool::then_some(guard). then_some is eager. On a *failed* claim it constructed the guard anyway and dropped it immediately, and the drop tried to take a lock the caller still held. The full test suite hung with no output. Both stuck processes showed zero CPU. then(|| guard) is lazy, and the difference between those two methods is now something I will never forget.

## Numbers

Per node, per full pass over the table, routed lookups are bounded by the number of distinct successor runs, which on a uniformly distributed ring is O(log N) in expectation. The lowest run costs zero lookups. Emissions are at least one second apart, and after k consecutive failures at least min(2^k, 60) seconds apart with full jitter on top. A fleet-wide restart spreads each node's first attempt over an eleven-second window keyed by a per-lifecycle UUID, so an identity cannot pre-select its time slot.

The cost the design bounds but does not remove is a peer that answers lookups and never admits a handshake. Each rotation spends one attempt on it, and that attempt can hold the single slot for up to the handshake timeout. Removing that cost means the transport remembering unreachable peers across attempts, which is a different piece of work.

## How it was checked

The state machine is a pure function: step(state, event) -> (state, actions). Clock readings and fresh UUIDs enter as event fields; the network, locks, and randomness live in the interpreter of the returned actions. That is not an aesthetic choice. It is what lets a finite model checker execute the *production* transition, not a hand-written shadow of it, across progress, invalid reports, late reports, cancellation, deferral, admission, loss, duplicates, churn, and restart, and check that request identifiers are never reused, that emissions respect the minimum interval, that a stale report changes nothing, and that a report cannot cause more than K+1 connection effects.

Alongside that: oracle tests that drive the transition against a known membership set and count lookups against the distinct-run bound; a three-node dummy-transport test that drains the entire admission cascade and caps it at twenty control submissions; and a real headless-Chrome test that a browser tab cannot start a second maintenance daemon while the first is still shutting down.

The pull request also ships a short paper in the repository’s papers/ directory that states all of the above as definitions, laws, and invariants, because prose like this post is not the durable form of a protocol. The PR went through five review rounds against its full diff, and the last two rounds found only abstraction nits. One of the CI failures along the way was the spell checker objecting to `{Identifiers}` in a BibTeX title, copied verbatim from the RFC. I let it win.

## What I would tell past me

Write the selection policy down as a sentence before writing it as code. “Lowest unverified first” reads as obviously right and is a starvation bug. “Round-robin over unverified slots, resuming after the last attempt” reads as slightly untidy and is the correct one.

Do not let a test oracle answer questions the network would not. Every one of the design bugs in this PR hid behind a harness that was too cooperative.

And when a Rust test suite hangs with no output and no CPU, go look for an eager then_some before you look anywhere else.

The change ships in Rings 0.26.0. It is a wire-format break, so an overlay upgrades together, and the finger table is finally a thing you can trust.
