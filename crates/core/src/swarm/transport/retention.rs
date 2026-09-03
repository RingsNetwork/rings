//! Connection retention under a bounded logical-connection capacity.
//!
//! State relation, per node `n`:
//! - `Admitted(n)` is the set of peers holding an `Active` lifecycle record.
//! - `Referenced(n, p)` iff `p` occupies a successor, predecessor, or finger
//!   slot of `n` ([`crate::dht::topology::TopologyState::references`]).
//! - `Age(n, p, now)` is how long `p`'s current generation has held a liveness
//!   record: since its data channel opened, since the first liveness scan or
//!   inbound payload that observed an admitted generation without one, or
//!   since the last repeated data-channel-open callback for the same
//!   generation, which restarts the record.
//! - `Idle(n, p, now)` is the time since the last authenticated inbound
//!   payload on that generation.
//! - `Dead(n, p)` iff `p`'s generation is send-terminal: an irrevocable send
//!   or delivery failure already revoked it, and stabilization will clean it.
//! - `Evictable(n, p, now)` iff `p ∈ Admitted(n) ∧ ¬Referenced(n, p) ∧
//!   (Dead(n, p) ∨ Age(n, p, now) >= UNREFERENCED_CONNECTION_GRACE_MS)`.
//! - `EvictionOrder`: dead generations first, then the most idle first.
//!
//! Invariant: `|lifecycle records of n| <= bounds.total()`, enforced by the
//! lifecycle registry. Eviction is the only policy-driven transition that
//! retires a healthy admitted connection; an explicit `disconnect` is
//! caller-driven. It runs only when the registry's own verdict says that one
//! retirement is what stands between a newcomer and admission, and it retires
//! at most one peer, so a full node recycles one displaced connection per
//! admission instead of rejecting every newcomer. The reference check and
//! the retirement share one lifecycle critical section, so a peer referenced
//! at retirement time is never evicted.
//!
//! Why the order is idleness rather than age: a physical connection is shared
//! by both endpoints while topology references are directed. A peer that `n`
//! no longer references may still hold `n` as its own finger or successor,
//! and `n` cannot observe that reference, but such a peer keeps sending
//! stabilization traffic. Idleness is the only local evidence of a connection
//! nobody uses; age would evict the longest-standing honest edges first.
//!
//! Why eviction is pressure-driven rather than periodic: closing every
//! locally unreferenced connection on a timer would sever live routing edges
//! of honest peers, which reconnect on their next finger fix, producing a
//! churn loop. Bounding the table and recycling the idlest connection only
//! when a newcomer needs the slot keeps every edge that nobody is competing
//! for. The physical close of the evicted connection runs inline on the
//! reservation path, bounded by the transport close timeout.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use super::connection::UnreferencedRetirement;
use super::pending::LifecycleBounds;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use super::pending::ReservationVerdict;
use super::pending::DEFAULT_PENDING_CONNECTION_CAPACITY;
use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::dht::Did;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::DISCONNECTED_CONNECTION_GRACE_MS;
use crate::error::Result;

/// Minimum liveness-record age before an unreferenced connection may be evicted.
///
/// A freshly admitted peer is still negotiating its role: its finger-fix
/// report or join continuation may reference it within one round trip. It is
/// the same grace a disconnected transport receives before stabilization
/// reclaims it: a connection gets equal patience whether it is silent or
/// unreferenced.
pub(crate) const UNREFERENCED_CONNECTION_GRACE_MS: i64 = DISCONNECTED_CONNECTION_GRACE_MS;

/// Logical connections retained per topology reference slot.
///
/// Law: `total = RETAINED_CONNECTIONS_PER_REFERENCE_SLOT × (finger slots +
/// successor capacity + 1 predecessor)`, where the finger slot count is
/// [`DEFAULT_FINGER_TABLE_SIZE`], one slot per ring bit, the upper bound on the
/// distinct fingers any peer can hold regardless of the locally configured
/// table size. The first share is a bound: this node references at most that
/// many peers. The second share is a heuristic, not a bound: Chord places no
/// limit on how many peers may hold this node as a finger, so the share
/// merely reserves room for the symmetric edges those peers keep open and
/// for application-initiated direct connections. Excess in-degree is handled
/// by the eviction order, not by the size. Pending handshakes count against
/// the same total.
const RETAINED_CONNECTIONS_PER_REFERENCE_SLOT: usize = 2;

/// Lifecycle bounds derived from the successor-list capacity of the local DHT.
pub(super) fn lifecycle_bounds(successor_capacity: usize) -> LifecycleBounds {
    let reference_slots = DEFAULT_FINGER_TABLE_SIZE
        .saturating_add(successor_capacity)
        .saturating_add(1);
    LifecycleBounds::new(
        DEFAULT_PENDING_CONNECTION_CAPACITY,
        reference_slots.saturating_mul(RETAINED_CONNECTIONS_PER_REFERENCE_SLOT),
    )
}

/// Liveness evidence for one admitted generation, when a record exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionEvidence {
    /// `Age(n, p, now)`.
    age_ms: i64,
    /// `Idle(n, p, now)`.
    idle_ms: i64,
}

/// One admitted generation with the facts the eviction order ranks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionCandidate {
    attempt: PendingConnectionAttempt,
    /// `Dead(n, p)`.
    send_terminal: bool,
    /// `None` when liveness holds no record for this generation; such a peer
    /// has not proven an open data channel and is treated as within grace.
    evidence: Option<RetentionEvidence>,
}

impl RetentionCandidate {
    /// `Evictable(n, p, now)` with `Age` and `Dead` already evaluated at plan time.
    fn is_evictable(&self, referenced: &BTreeSet<Did>) -> bool {
        if referenced.contains(&self.attempt.peer) {
            return false;
        }
        self.send_terminal
            || self
                .evidence
                .is_some_and(|evidence| evidence.age_ms >= UNREFERENCED_CONNECTION_GRACE_MS)
    }

    /// Rank key of `EvictionOrder`: dead first, then most idle first.
    fn eviction_rank(&self) -> (Reverse<bool>, Reverse<i64>) {
        (
            Reverse(self.send_terminal),
            Reverse(self.evidence.map_or(0, |evidence| evidence.idle_ms)),
        )
    }
}

/// `EvictionOrder(A, R)`: the evictable candidates, dead first, then most idle.
///
/// Post: every returned attempt is unreferenced by `referenced` and either
/// dead or past grace; ties keep the caller's order, which is DID order for
/// registry projections, so the plan is deterministic for one snapshot.
fn eviction_order(
    candidates: Vec<RetentionCandidate>,
    referenced: &BTreeSet<Did>,
) -> Vec<PendingConnectionAttempt> {
    let mut evictable = candidates
        .into_iter()
        .filter(|candidate| candidate.is_evictable(referenced))
        .collect::<Vec<_>>();
    evictable.sort_by_key(RetentionCandidate::eviction_rank);
    evictable
        .into_iter()
        .map(|candidate| candidate.attempt)
        .collect()
}

impl SwarmTransport {
    /// Whether retiring one admitted record is what stands between `peer` and
    /// a reservation, per the registry's own verdict.
    pub(super) fn reservation_needs_eviction(&self, peer: Did) -> Result<bool> {
        Ok(self
            .peer_lifecycles()?
            .reservation_verdict(peer)
            .needs_eviction())
    }

    /// Snapshot the eviction plan under the lifecycle boundary.
    ///
    /// Topology references and liveness evidence are read under one lock so a
    /// concurrent admission or retirement cannot interleave with the plan.
    fn unreferenced_eviction_order(&self, now_ms: i64) -> Result<Vec<PendingConnectionAttempt>> {
        self.with_connection_lifecycle(|| {
            let referenced = self.dht.topology_state()?.referenced_peers();
            let lifecycles = self.peer_lifecycles()?;
            let liveness = self.peer_liveness()?;
            let candidates = lifecycles
                .admitted_connections()
                .iter()
                .map(|attempt| RetentionCandidate {
                    attempt,
                    send_terminal: lifecycles.is_send_terminal(attempt),
                    evidence: liveness
                        .connected_for_ms(attempt.peer, attempt.generation, now_ms)
                        .zip(liveness.idle_for_ms(attempt.peer, attempt.generation, now_ms))
                        .map(|(age_ms, idle_ms)| RetentionEvidence { age_ms, idle_ms }),
                })
                .collect();
            Ok(eviction_order(candidates, &referenced))
        })
    }

    /// Retire the first evictable admitted connection to free one slot.
    ///
    /// Pre: the registry verdict for the reserving peer is `CapacityExceeded`.
    /// Post: at most one admitted generation that was `Evictable` at plan
    /// time, and is still unreferenced inside the retirement critical
    /// section, left the DHT and had its transport closed. Referenced and
    /// younger connections are preserved, so a plan without survivors leaves
    /// the reservation to be rejected by the registry bound. A candidate
    /// referenced by a topology transition after the plan, or whose
    /// generation was superseded, is skipped in favour of the next one.
    pub(super) async fn evict_unreferenced_connection(&self, now_ms: i64) -> Result<()> {
        self.evict_unreferenced_connection_with(now_ms, |_| {})
            .await
    }

    async fn evict_unreferenced_connection_with(
        &self,
        now_ms: i64,
        observe_plan: impl FnOnce(&[PendingConnectionAttempt]),
    ) -> Result<()> {
        let plan = self.unreferenced_eviction_order(now_ms)?;
        observe_plan(&plan);
        for attempt in plan {
            match self.retire_unless_referenced(attempt).await? {
                UnreferencedRetirement::Referenced | UnreferencedRetirement::Superseded => {
                    continue;
                }
                UnreferencedRetirement::Retired => {}
            }
            tracing::info!(
                target: "rings_core::swarm::transport::retention",
                local = %self.dht.did,
                peer = %attempt.peer,
                generation = attempt.generation,
                grace_ms = UNREFERENCED_CONNECTION_GRACE_MS,
                "evicted unreferenced connection to admit a new peer"
            );
            return Ok(());
        }
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn evict_unreferenced_connection_with_plan_observer_for_test(
        &self,
        now_ms: i64,
        observe_plan: impl FnOnce(&[PendingConnectionAttempt]),
    ) -> Result<()> {
        self.evict_unreferenced_connection_with(now_ms, observe_plan)
            .await
    }

    /// Replace the lifecycle bounds of an empty registry.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn set_lifecycle_bounds_for_test(&self, bounds: LifecycleBounds) -> Result<()> {
        self.with_connection_lifecycle(|| {
            self.peer_lifecycles()?.set_bounds_for_test(bounds);
            Ok(())
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn reservation_verdict_for_test(&self, peer: Did) -> Result<ReservationVerdict> {
        Ok(self.peer_lifecycles()?.reservation_verdict(peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn candidate(
        peer: Did,
        generation: u64,
        send_terminal: bool,
        evidence: Option<(i64, i64)>,
    ) -> RetentionCandidate {
        RetentionCandidate {
            attempt: PendingConnectionAttempt { peer, generation },
            send_terminal,
            evidence: evidence.map(|(age_ms, idle_ms)| RetentionEvidence { age_ms, idle_ms }),
        }
    }

    fn peer() -> Did {
        SecretKey::random().address().into()
    }

    /// Law: the total grows by exactly one share per extra successor slot,
    /// the handshake share always fits inside it, and the bound never drops
    /// below both directions of the finger and predecessor slots.
    #[test]
    fn test_bounds_grow_by_one_share_per_successor_slot_and_contain_the_handshake_share() {
        for successor_capacity in 0..8 {
            let bounds = lifecycle_bounds(successor_capacity);
            let next = lifecycle_bounds(successor_capacity + 1);
            assert_eq!(
                next.total() - bounds.total(),
                RETAINED_CONNECTIONS_PER_REFERENCE_SLOT
            );
            assert!(bounds.pending() <= bounds.total());
            assert!(
                bounds.total()
                    >= RETAINED_CONNECTIONS_PER_REFERENCE_SLOT * (DEFAULT_FINGER_TABLE_SIZE + 1)
            );
        }
        assert_eq!(
            lifecycle_bounds(3).pending(),
            DEFAULT_PENDING_CONNECTION_CAPACITY
        );
    }

    #[test]
    fn test_eviction_order_keeps_referenced_young_and_unproven_peers() {
        let referenced_peer = peer();
        let young = peer();
        let unproven = peer();
        let referenced = BTreeSet::from([referenced_peer]);
        let old = peer();

        let order = eviction_order(
            vec![
                candidate(
                    referenced_peer,
                    1,
                    false,
                    Some((UNREFERENCED_CONNECTION_GRACE_MS * 4, 0)),
                ),
                candidate(
                    young,
                    2,
                    false,
                    Some((UNREFERENCED_CONNECTION_GRACE_MS - 1, 0)),
                ),
                candidate(unproven, 3, false, None),
                candidate(old, 4, false, Some((UNREFERENCED_CONNECTION_GRACE_MS, 0))),
            ],
            &referenced,
        );

        assert_eq!(order, vec![PendingConnectionAttempt {
            peer: old,
            generation: 4
        }]);
    }

    /// Law: dead generations precede live ones regardless of age or idleness,
    /// and live ones are ranked by idleness, not by age.
    #[test]
    fn test_eviction_order_ranks_dead_first_then_most_idle() {
        let dead_young = peer();
        let old_but_active = peer();
        let young_but_idle = peer();
        let grace = UNREFERENCED_CONNECTION_GRACE_MS;

        let order = eviction_order(
            vec![
                candidate(old_but_active, 1, false, Some((grace * 10, 1_000))),
                candidate(young_but_idle, 2, false, Some((grace, grace * 2))),
                candidate(dead_young, 3, true, Some((grace / 2, 0))),
            ],
            &BTreeSet::new(),
        );

        assert_eq!(
            order
                .into_iter()
                .map(|attempt| attempt.generation)
                .collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
    }

    #[test]
    fn test_eviction_order_is_stable_for_equal_ranks() {
        let mut peers = (0..3).map(|_| peer()).collect::<Vec<_>>();
        peers.sort();
        let candidates = peers
            .iter()
            .enumerate()
            .map(|(index, peer)| {
                candidate(
                    *peer,
                    index as u64,
                    false,
                    Some((UNREFERENCED_CONNECTION_GRACE_MS, 0)),
                )
            })
            .collect::<Vec<_>>();

        let order = eviction_order(candidates.clone(), &BTreeSet::new());

        assert_eq!(
            order,
            candidates
                .iter()
                .map(|candidate| candidate.attempt)
                .collect::<Vec<_>>()
        );
    }
}
