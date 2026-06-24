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
//!       mutates it through an N-slot ROTATING index — sequential state, not a
//!       single join. Production uses 160 slots; this controlled schedule uses a
//!       smaller configured table to keep the async integration test bounded
//!       while still asserting the full configured DHT state.
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
//! asserted bounded-time convergence — unsound without `correct_stabilize`.
//! A schedule that did NOT converge would be a reproducible
//! bug, pinned to its exact delivery sequence.
//! ====================================================================

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fmt::Write as _;
    use std::sync::Arc;

    use rings_transport::connections::dummy_controlled;

    use crate::dht::Chord;
    use crate::dht::PeerRing;
    use crate::ecc::SecretKey;
    use crate::inspect::DHTInspect;
    use crate::session::SessionSk;
    use crate::storage::MemStorage;
    use crate::swarm::Swarm;
    use crate::swarm::SwarmBuilder;
    use crate::tests::default::Node;
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

    /// The schedule still drives the production async DHT path and asserts the
    /// full configured DHT state; using 16 finger slots keeps the test focused on
    /// topology/finger convergence instead of spending most time rotating 160
    /// production slots.
    const SCHEDULE_FINGER_TABLE_SIZE: usize = 16;
    /// Stabilize rounds before giving up. With 16 configured finger slots these
    /// schedules should reach the fixpoint far inside this bound; tripping it is
    /// a reproducible non-convergence.
    const MAX_ROUNDS: usize = 120;
    /// Guard against a routing self-route delivery loop (non-termination).
    const MAX_DELIVERIES: usize = 2_000_000;
    /// Guard against a single stabilization round that keeps producing messages
    /// forever.
    const MAX_DELIVERIES_PER_ROUND: usize = 100_000;
    /// Guard against a schedule that is still delivering messages but no longer
    /// changes any DHT state.
    const MAX_STAGNANT_ROUNDS: usize = 20;
    /// Number of recent round summaries retained for failure diagnostics.
    const TRACE_WINDOW: usize = 16;

    struct ControlledDeliveryGuard;

    impl ControlledDeliveryGuard {
        fn enable() -> Self {
            dummy_controlled::enable(true);
            Self
        }
    }

    impl Drop for ControlledDeliveryGuard {
        fn drop(&mut self) {
            dummy_controlled::enable(false);
        }
    }

    #[derive(Debug, Default)]
    struct DrainStats {
        deliveries: usize,
        dropped: usize,
        max_pending: usize,
    }

    #[derive(Debug)]
    struct RoundTrace {
        phase: String,
        deliveries: usize,
        dropped: usize,
        total_delivered: usize,
        max_pending: usize,
        changed: bool,
        topology_matches: usize,
        full_matches: usize,
        finger_known_slots: Vec<u64>,
        first_mismatch: String,
    }

    #[derive(Debug, Default)]
    struct ScheduleDiagnostics {
        traces: VecDeque<RoundTrace>,
    }

    impl ScheduleDiagnostics {
        fn push(&mut self, trace: RoundTrace) {
            if self.traces.len() == TRACE_WINDOW {
                self.traces.pop_front();
            }
            self.traces.push_back(trace);
        }

        fn render(&self, actual: &[DHTInspect], expected: &[DHTInspect]) -> String {
            let mut out = String::new();
            let _ = writeln!(
                out,
                "finger_table_size={SCHEDULE_FINGER_TABLE_SIZE}, max_rounds={MAX_ROUNDS}, \
                 max_deliveries={MAX_DELIVERIES}, max_deliveries_per_round={MAX_DELIVERIES_PER_ROUND}"
            );
            let _ = writeln!(out, "recent rounds:");
            for trace in &self.traces {
                let _ = writeln!(
                    out,
                    "  {phase}: delivered={delivered} dropped={dropped} total={total} \
                     max_pending={max_pending} changed={changed} topology={topology}/{nodes} \
                     full={full}/{nodes} finger_known_slots={finger:?} first_mismatch={mismatch}",
                    phase = trace.phase,
                    delivered = trace.deliveries,
                    dropped = trace.dropped,
                    total = trace.total_delivered,
                    max_pending = trace.max_pending,
                    changed = trace.changed,
                    topology = trace.topology_matches,
                    nodes = expected.len(),
                    full = trace.full_matches,
                    finger = trace.finger_known_slots,
                    mismatch = trace.first_mismatch,
                );
            }
            let _ = writeln!(out, "current first mismatch:");
            let _ = write!(out, "{}", describe_first_mismatch(actual, expected));
            let _ = writeln!(out, "current topology mismatches:");
            let _ = write!(out, "{}", describe_topology_mismatches(actual, expected));
            out
        }
    }

    fn inspect_all(swarms: &[Arc<Swarm>]) -> Vec<DHTInspect> {
        swarms
            .iter()
            .map(|swarm| DHTInspect::inspect(&swarm.dht()))
            .collect()
    }

    fn topology_matches(actual: &DHTInspect, expected: &DHTInspect) -> bool {
        actual.successors == expected.successors && actual.predecessor == expected.predecessor
    }

    fn known_finger_slots(dht: &DHTInspect) -> u64 {
        dht.finger_table
            .iter()
            .filter(|(did, _, _)| did.is_some())
            .map(|(_, start, end)| end - start + 1)
            .sum()
    }

    fn short_did(did: &str) -> &str {
        did.get(..10).unwrap_or(did)
    }

    fn mismatch_summary(actual: &[DHTInspect], expected: &[DHTInspect]) -> String {
        actual
            .iter()
            .zip(expected)
            .enumerate()
            .find(|(_, (act, exp))| act != exp)
            .map(|(i, (act, exp))| {
                let topic = if !topology_matches(act, exp) {
                    "topology"
                } else {
                    "finger"
                };
                format!("node{i}/{}:{topic}", short_did(&act.did))
            })
            .unwrap_or_else(|| "none".to_string())
    }

    fn describe_first_mismatch(actual: &[DHTInspect], expected: &[DHTInspect]) -> String {
        for (i, (act, exp)) in actual.iter().zip(expected).enumerate() {
            if act == exp {
                continue;
            }
            return format!(
                "node{i} {}\nactual:   successors={:?} predecessor={:?} finger={:?}\n\
                 expected: successors={:?} predecessor={:?} finger={:?}\n",
                act.did,
                act.successors,
                act.predecessor,
                act.finger_table,
                exp.successors,
                exp.predecessor,
                exp.finger_table,
            );
        }
        "none\n".to_string()
    }

    fn describe_topology_mismatches(actual: &[DHTInspect], expected: &[DHTInspect]) -> String {
        let mut out = String::new();
        for (i, (act, exp)) in actual.iter().zip(expected).enumerate() {
            if topology_matches(act, exp) {
                continue;
            }
            let _ = writeln!(
                out,
                "node{i} {} actual_succ={:?} actual_pred={:?} expected_succ={:?} expected_pred={:?}",
                short_did(&act.did),
                act.successors,
                act.predecessor,
                exp.successors,
                exp.predecessor,
            );
        }
        if out.is_empty() {
            out.push_str("none\n");
        }
        out
    }

    fn round_trace(
        phase: impl Into<String>,
        stats: DrainStats,
        total_delivered: usize,
        before: &[DHTInspect],
        actual: &[DHTInspect],
        expected: &[DHTInspect],
    ) -> RoundTrace {
        RoundTrace {
            phase: phase.into(),
            deliveries: stats.deliveries,
            dropped: stats.dropped,
            total_delivered,
            max_pending: stats.max_pending,
            changed: before != actual,
            topology_matches: actual
                .iter()
                .zip(expected)
                .filter(|(act, exp)| topology_matches(act, exp))
                .count(),
            full_matches: actual
                .iter()
                .zip(expected)
                .filter(|(act, exp)| act == exp)
                .count(),
            finger_known_slots: actual.iter().map(known_finger_slots).collect(),
            first_mismatch: mismatch_summary(actual, expected),
        }
    }

    async fn prepare_schedule_node(key: SecretKey) -> Node {
        let stun = "stun://stun.l.google.com:19302";
        let storage = Box::new(MemStorage::new());
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        let swarm = Arc::new(
            SwarmBuilder::new(0, stun, storage, session_sk)
                .dht_finger_table_size(SCHEDULE_FINGER_TABLE_SIZE)
                .build(),
        );

        println!("key: {:?}", key.to_string());
        println!("did: {:?}", swarm.did());

        Node::new(swarm)
    }

    fn gen_schedule_dht(did: crate::dht::Did) -> PeerRing {
        let storage = Box::new(MemStorage::new());
        PeerRing::new_with_storage_and_finger_table_size(
            did,
            3,
            storage,
            SCHEDULE_FINGER_TABLE_SIZE,
        )
    }

    /// The unique converged DHT each node must reach, built via the production
    /// join/notify path over the full DID set — the same fixpoint checked by
    /// `dht_convergence`.
    fn expected_dhts(swarms: &[Arc<Swarm>]) -> Vec<DHTInspect> {
        swarms
            .iter()
            .map(|swarm| {
                let dht = gen_schedule_dht(swarm.did());
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

    /// Drain the controlled queue to quiescence, choosing the next index via
    /// `pick` (the delivery-order strategy under test).
    async fn drain(pick: fn(usize) -> usize, delivered: &mut usize) -> DrainStats {
        let mut stats = DrainStats::default();
        while dummy_controlled::pending() > 0 {
            let pending = dummy_controlled::pending();
            stats.max_pending = stats.max_pending.max(pending);
            let idx = pick(pending);
            if !dummy_controlled::deliver(idx).await {
                stats.dropped += 1;
            }
            *delivered += 1;
            stats.deliveries += 1;
            assert!(
                *delivered < MAX_DELIVERIES,
                "runaway delivery - likely a routing self-route loop"
            );
            assert!(
                stats.deliveries < MAX_DELIVERIES_PER_ROUND,
                "round delivery budget exceeded: delivered {} in one drain, max_pending {}",
                stats.deliveries,
                stats.max_pending
            );
        }
        stats
    }

    /// Drive the full 6-node bootstrap + stabilization under ONE controlled
    /// delivery order, draining to quiescence each round, then assert convergence
    /// to the unique fixpoint. Fully deterministic: no timers, no wall clock —
    /// the only nondeterminism the integration test had (random per-message delay)
    /// is replaced by an explicit, reproducible delivery order.
    async fn run_schedule(pick: fn(usize) -> usize) {
        let mut nodes = vec![];
        for k in KEYS {
            nodes.push(prepare_schedule_node(SecretKey::try_from(k).unwrap()).await);
        }
        let swarms: Vec<Arc<Swarm>> = nodes.iter().map(|n| n.swarm.clone()).collect();
        let expected = expected_dhts(&swarms);
        let mut diagnostics = ScheduleDiagnostics::default();

        let _controlled = ControlledDeliveryGuard::enable();

        // Star bootstrap - queues each connection's setup events.
        for sw in swarms.iter().skip(1) {
            manually_establish_connection(&swarms[0], sw).await;
        }

        let mut delivered = 0usize;
        let before_bootstrap = inspect_all(&swarms);
        let bootstrap_stats = drain(pick, &mut delivered).await;
        let mut actual = inspect_all(&swarms);
        diagnostics.push(round_trace(
            "bootstrap",
            bootstrap_stats,
            delivered,
            &before_bootstrap,
            &actual,
            &expected,
        ));

        let mut ok = false;
        let mut stagnant_rounds = 0usize;
        for round in 1..=MAX_ROUNDS {
            let before = actual.clone();
            for sw in &swarms {
                let _ = sw.stabilizer().stabilize().await;
            }
            let stats = drain(pick, &mut delivered).await;
            actual = inspect_all(&swarms);
            let changed = before != actual;
            diagnostics.push(round_trace(
                format!("round{round}"),
                stats,
                delivered,
                &before,
                &actual,
                &expected,
            ));

            if actual == expected {
                ok = true;
                break;
            }
            if changed {
                stagnant_rounds = 0;
            } else {
                stagnant_rounds += 1;
                assert!(
                    stagnant_rounds < MAX_STAGNANT_ROUNDS,
                    "schedule made no DHT progress for {stagnant_rounds} rounds\n{}",
                    diagnostics.render(&actual, &expected)
                );
            }
        }

        assert!(
            ok,
            "did not converge under the chosen delivery schedule\n{}",
            diagnostics.render(&actual, &expected)
        );
        for (i, (act, exp)) in actual.iter().zip(&expected).enumerate() {
            pretty_assertions::assert_eq!(act, exp, "node{i}");
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
