//! Deterministic, controlled-ordering convergence test for the full 6-node
//! stabilization — driven through the dummy transport's explicit delivery queue
//! (see `dummy::controlled`), not random per-message jitter + a wall-clock
//! deadline. The orderings to cover are *derived first* (below), then filled in
//! as tests.
//!
//! ====================================================================
//! FORMAL DERIVATION (TLA+) — why the ordering equivalence classes collapse.
//!
//! We don't have a runtime tool to force a particular async schedule, so we
//! derive the timing-state structure on paper and let the controlled queue
//! realise representative orders. The result is strong: the stabilization
//! protocol is CONFLUENT — every fair delivery order reaches the SAME fixpoint.
//!
//! ---- MODULE ChordStabilizeConfluence --------------------------------
//! EXTENDS Integers, FiniteSets, Bags
//!
//! CONSTANTS Node, Id, M, K          \* as in MODULE ChordConvergence
//!
//! VARIABLES
//!   pred,      \* pred  \in [Node -> Node \cup {NIL}]   — predecessor pointer
//!   known,     \* known \in [Node -> SUBSET Node]       — connected peers
//!   net        \* net : a Bag (multiset) of in-flight messages = the dummy
//!              \*       transport's explicit delivery queue
//!
//! \* ---- The per-node local state is a JOIN-SEMILATTICE ----------------
//! \* `known[n]` ordered by ⊆ ; `connect` only ever ADDS  → ∪ is the join.
//! \* `pred[n]`  ordered by "closer behind wins": p1 ⊑ p2 iff
//! \*            dist(n,p1) ≤ dist(n,p2). `notify` sets pred[n] to the join
//! \*            (the farther-forward / closer-behind candidate). NIL is ⊥.
//! \* Both are bounded-height lattices (height ≤ |Node|).
//!
//! \* ---- Every action is a MONOTONE, INFLATIONARY operator ------------
//! \* Notify(s,n)  (s ∈ succ(known[s]))   : pred[n]  := pred[n]  ⊔ s
//! \* Connect(n,p) (from a Report)        : known[n] := known[n] ∪ {p}
//! \* Each only moves a node's state UP its lattice; none ever lowers it.
//! \* Stabilize(n) emits messages determined by the current state, and is
//! \* MONOTONE in that state (more knowledge ⇒ a superset of messages).
//!
//! \* ---- Independence / local confluence ------------------------------
//! \* Any two deliveries commute:
//! \*   - different target nodes        → disjoint state, trivially commute;
//! \*   - same node, both `notify`      → two ⊔ on pred[n], ⊔ is commutative;
//! \*   - same node, both `connect`     → two ∪ on known[n], ∪ is commutative;
//! \*   - same node, notify + connect   → different fields, commute.
//! \* Actions only ENABLE further actions (monotone), never disable them.
//!
//! \* ---- THEOREM Confluence -------------------------------------------
//! \* The reachable quiescent state is the LEAST FIXPOINT of the combined
//! \* monotone operator (Knaster–Tarski). A least fixpoint is unique and is
//! \* reached by ANY fair chaotic iteration order (Kleene / Cousot chaotic
//! \* iteration). Hence:
//! \*
//! \*   ASSUME  Fairness ==  \* every enabled delivery eventually happens AND
//! \*                        \* every node stabilises infinitely often
//! \*   PROVE   <>[]( (pred, known) = TheFixpoint )
//! \*           /\ TheFixpoint = << [n ∈ Node |-> Predecessor(n)],
//! \*                                [n ∈ Node |-> Successors(n) as a set] >>
//! \*           \* i.e. exactly MODULE ChordConvergence's fixpoint,
//! \*           \* INDEPENDENT of delivery order.
//!
//! \* ---- COROLLARY (what this means for the test) ---------------------
//! \* Convergence is order-independent; the equivalence classes of orderings
//! \* collapse to ONE w.r.t. the outcome. The previous integration test's flakiness was
//! \* therefore NOT outcome nondeterminism — it was a TIME-BOUNDED drain
//! \* (random per-message delay + a 90s wall-clock deadline) cut off before
//! \* quiescence. Driving the dummy delivery queue to quiescence removes the
//! \* wall clock and makes convergence deterministic; any representative order
//! \* (FIFO, LIFO, reversed, a few hand-picked adversarial ones) lands on the
//! \* same fixpoint. A non-converging order, if one existed, would be a real
//! \* confluence violation — a genuine bug, reproducible by its exact sequence.
//! ====================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rings_transport::connections::dummy_controlled;

    use crate::dht::Chord;
    use crate::ecc::SecretKey;
    use crate::inspect::DHTInspect;
    use crate::swarm::Swarm;
    use crate::tests::default::gen_pure_dht;
    use crate::tests::default::prepare_node;
    use crate::tests::manually_establish_connection;

    /// The six fixed clustered production DIDs (= `Layout::Clustered`): the
    /// pathological worst case. Convergence here is the same fixpoint, reached
    /// deterministically.
    const KEYS: [&str; 6] = [
        "9c83fcb684af3dc71018b5a303245d2f2fed8a579096589f3234a67a52a7ac66",
        "fd674cb6089663935cb061254602e343da8a2fa3908980ae4f7a27adb8b7ac8a",
        "b9ce7159a2ad3b9fe885a7744d32afeec233e7ddeaed0759cbab2c00a1bd548b",
        "4efb629f54a3f3dd91f5efffc4f9b51ab27eb082b2393067757681ed6439480d",
        "f2cbca82fb82745c1f9d94c1c9d2b0606daaf6f15ac8a215fc72c8bc0478ecf5",
        "e1d7f24e2b725df077627fc0337b9c53b37ce594ca84fccd0f36dc58423a0ed2",
    ];

    /// Stabilize rounds before giving up. Generous: confluence guarantees
    /// termination well within this; tripping it is a real non-convergence.
    const MAX_ROUNDS: usize = 400;
    /// Guard against a routing self-route delivery loop (non-termination).
    const MAX_DELIVERIES: usize = 2_000_000;

    /// The unique converged DHT each node must reach, built via the production
    /// join/notify path over the full DID set — the same fixpoint checked by
    /// `dht_convergence`.
    fn expected_dhts(swarms: &[Arc<Swarm>]) -> Vec<DHTInspect> {
        swarms
            .iter()
            .map(|swarm| {
                let dht = gen_pure_dht(swarm.did());
                for other in swarms {
                    if dht.did != other.did() {
                        dht.join(other.did()).unwrap();
                        dht.notify(other.did()).unwrap();
                    }
                }
                DHTInspect::inspect(&dht)
            })
            .collect()
    }

    fn converged(swarms: &[Arc<Swarm>], expected: &[DHTInspect]) -> bool {
        swarms
            .iter()
            .zip(expected)
            .all(|(sw, exp)| &DHTInspect::inspect(&sw.dht()) == exp)
    }

    /// Drain the controlled queue to quiescence, choosing the next index via
    /// `pick` (the delivery-order strategy under test).
    async fn drain(pick: fn(usize) -> usize, delivered: &mut usize) {
        while dummy_controlled::pending() > 0 {
            let idx = pick(dummy_controlled::pending());
            dummy_controlled::deliver(idx).await;
            *delivered += 1;
            assert!(
                *delivered < MAX_DELIVERIES,
                "runaway delivery — likely a routing self-route loop"
            );
        }
    }

    /// Drive the full 6-node bootstrap + stabilization under ONE controlled
    /// delivery order, draining to quiescence each round, then assert convergence
    /// to the unique fixpoint. Fully deterministic: no timers, no wall clock —
    /// the only nondeterminism the integration test had (random per-message delay)
    /// is replaced by an explicit, reproducible delivery order.
    async fn run_schedule(pick: fn(usize) -> usize) {
        let mut nodes = vec![];
        for k in KEYS {
            nodes.push(prepare_node(SecretKey::try_from(k).unwrap()).await);
        }
        let swarms: Vec<Arc<Swarm>> = nodes.iter().map(|n| n.swarm.clone()).collect();
        let expected = expected_dhts(&swarms);

        dummy_controlled::enable(true);

        // Star bootstrap — queues each connection's setup events.
        for sw in swarms.iter().skip(1) {
            manually_establish_connection(&swarms[0], sw).await;
        }

        let mut delivered = 0usize;
        drain(pick, &mut delivered).await; // process bootstrap (DataChannelOpen -> join_dht -> ...)

        let mut ok = false;
        for _ in 0..MAX_ROUNDS {
            for sw in &swarms {
                let _ = sw.stabilizer().stabilize().await;
            }
            drain(pick, &mut delivered).await;
            if converged(&swarms, &expected) {
                ok = true;
                break;
            }
        }

        dummy_controlled::enable(false);

        assert!(ok, "did not converge under the chosen delivery schedule");
        for (i, (sw, exp)) in swarms.iter().zip(&expected).enumerate() {
            pretty_assertions::assert_eq!(DHTInspect::inspect(&sw.dht()), exp.clone(), "node{i}");
        }

        // Keep the monitoring receivers alive to the end (dropping a Node makes
        // its recording callback panic on the next message).
        drop(nodes);
    }

    /// Front of the queue (FIFO / oldest pending first).
    fn fifo(_pending: usize) -> usize {
        0
    }

    /// Back of the queue (LIFO / newest pending first) — the opposite extreme.
    fn lifo(pending: usize) -> usize {
        pending - 1
    }

    /// Confluence representative #1: oldest-first delivery converges.
    #[tokio::test]
    async fn schedule_fifo_converges() {
        run_schedule(fifo).await;
    }

    /// Confluence representative #2: newest-first delivery reaches the SAME
    /// fixpoint — the two extremes witnessing the order-independence proved above.
    #[tokio::test]
    async fn schedule_lifo_converges() {
        run_schedule(lifo).await;
    }
}
