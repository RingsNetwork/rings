//! Deterministic, controlled-ordering convergence test for the full 6-node
//! stabilization — driven through the dummy transport's explicit delivery queue
//! (see `dummy::controlled`), not random per-message jitter + a wall-clock
//! deadline. The orderings are reasoned about first (below), then filled in as
//! tests.
//!
//! ====================================================================
//! DERIVATION — what is order-independent here, and what these tests check.
//!
//! We have no runtime tool to force a particular async schedule, so we reason
//! about the timing-state structure on paper and let the controlled queue
//! realise representative orders.
//!
//! ---- The FIXPOINT is unique (order-independent) --------------------
//! The asserted converged state of a node — its successor list, predecessor AND
//! finger table — is a DETERMINISTIC FUNCTION of the set of peers it knows:
//!   Successors(n) = the K closest forward nodes of `known[n]`
//!   Predecessor(n) = the closest node behind n among those that notify it
//!   Finger(n,k)   = the no-wrap finger rule over `known[n]`
//! This is exactly `MODULE ChordConvergence` (dht_convergence.rs) evaluated at a
//! given `known[n]`. `known[n]` only grows (connections are not dropped on the
//! happy path) and, for these six fully-discoverable DIDs, converges to "all
//! other nodes". Once `known[n]` = all, those three functions each evaluate to a
//! single value — so the fixpoint is unique and independent of delivery order.
//!
//! ---- Why this is NOT a full all-orders confluence proof ------------
//! Reaching that fixpoint is liveness, and the production path is more than a
//! monotone lattice in two ways this derivation deliberately does NOT model — so
//! a blanket Knaster–Tarski / "every fair order converges" claim is unjustified:
//!   (1) `notify_predecessor` emits to `successors().list()`, TRUNCATED to K:
//!       when a closer peer is learned, a farther successor can drop OUT of the
//!       top-K, so the set of notify targets is not monotone — "more knowledge
//!       ⇒ more messages" is false. (`pred` still converges, because a node's
//!       immediate predecessor always keeps it as successor #1 and so never
//!       stops notifying it — but that is a separate argument, not monotonicity.)
//!   (2) the finger table is part of the asserted state, and `fix_fingers`
//!       mutates it through a 160-slot ROTATING index — sequential state, not a
//!       single join.
//! A rigorous all-orders theorem would have to model truncation + the finger
//! actions; we do not claim it. Exhaustive interleaving exploration lives in the
//! stage-2 Stateright model (on its abstraction).
//!
//! ---- What the tests below actually check --------------------------
//! Two REPRESENTATIVE deterministic schedules — FIFO (oldest pending first) and
//! LIFO (newest first), the two extremes — each drive the six clustered DIDs to
//! quiescence and reach the SAME unique fixpoint, with no timers / wall clock.
//! That is reproducible evidence that convergence is insensitive to delivery
//! order for this regime (not a proof over all orders). It replaces the old
//! wall-clock-bounded flaky `test_stabilization_final_dht`, whose 90s deadline
//! asserted bounded-time convergence — unsound without `correct_stabilize`
//! (experimental/off). A schedule that did NOT converge would be a reproducible
//! bug, pinned to its exact delivery sequence.
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

    /// Stabilize rounds before giving up. Generous: these schedules reach the
    /// fixpoint far inside this bound; tripping it is a real non-convergence.
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

    /// Representative schedule #1: oldest-first delivery converges.
    #[tokio::test]
    async fn schedule_fifo_converges() {
        run_schedule(fifo).await;
    }

    /// Representative schedule #2: newest-first delivery reaches the SAME
    /// fixpoint — the two extremes giving reproducible evidence of the
    /// order-insensitivity reasoned about above (not a proof over all orders).
    #[tokio::test]
    async fn schedule_lifo_converges() {
        run_schedule(lifo).await;
    }
}
