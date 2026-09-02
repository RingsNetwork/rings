//! Connection retention under a bounded logical-connection capacity.
//!
//! State relation, per node `n`:
//! - `Admitted(n)` is the set of peers holding an `Active` lifecycle record.
//! - `Referenced(n, p)` iff `p` occupies a successor, predecessor, or finger
//!   slot of `n` ([`crate::dht::topology::TopologyState::references`]), or `p`
//!   owns a storage placement that `n` replicates
//!   (`PeerRing::peer_may_share_storage_responsibility`).
//! - `Unreferenced(n, p)` iff `p ∈ Admitted(n) ∧ ¬Referenced(n, p)`.
//! - `Evictable(n, p, now)` iff `Unreferenced(n, p)` and `p`'s data channel has
//!   been open for at least [`UNREFERENCED_CONNECTION_GRACE_MS`].
//!
//! Invariant: `|lifecycle records of n| <= capacity`, enforced by the lifecycle
//! registry. Eviction is the only transition that retires a healthy admitted
//! connection, and it runs only when a reservation would otherwise violate the
//! bound: it removes exactly the oldest evictable peer, so a full node recycles
//! one displaced connection per admission instead of rejecting every newcomer.
//!
//! Why eviction is pressure-driven rather than periodic: a physical connection
//! is shared by both endpoints while topology references are directed. A peer
//! that `n` no longer references may still hold `n` as its own finger or
//! successor, and `n` cannot observe that. Closing every locally unreferenced
//! connection on a timer would therefore sever live routing edges of honest
//! peers, which reconnect on their next finger fix, producing a churn loop.
//! Bounding the table and recycling the least valuable connection only when a
//! newcomer needs the slot keeps every edge that nobody is competing for.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::dht::Did;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::error::Result;

/// Minimum data-channel age before an unreferenced connection may be evicted.
///
/// A freshly admitted peer is still negotiating its role: its finger-fix
/// report or join continuation may reference it within one round trip. The
/// grace mirrors the disconnected-transport grace used by stabilization.
pub(crate) const UNREFERENCED_CONNECTION_GRACE_MS: i64 = 30_000;

/// Logical connections retained per topology reference slot.
///
/// Law: `capacity = RETAINED_CONNECTIONS_PER_REFERENCE_SLOT × (finger slots +
/// successor capacity + 1 predecessor)`, where the finger slot count is
/// [`DEFAULT_FINGER_TABLE_SIZE`], one slot per ring bit, the upper bound on the
/// distinct fingers any peer can hold regardless of the locally configured
/// table size. The first share covers the peers this node references. The
/// second covers the symmetric edges held by peers that reference this node,
/// invisible locally because references are directed while connections are
/// shared, together with application-initiated direct connections. Pending
/// handshakes count against the same bound.
const RETAINED_CONNECTIONS_PER_REFERENCE_SLOT: usize = 2;

/// Upper bound on peers holding any lifecycle record at one node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConnectionCapacity(usize);

impl ConnectionCapacity {
    /// Derive the capacity from the configured successor-list capacity.
    pub(crate) const fn for_successor_capacity(successor_capacity: u8) -> Self {
        let reference_slots = DEFAULT_FINGER_TABLE_SIZE
            .saturating_add(successor_capacity as usize)
            .saturating_add(1);
        Self(reference_slots.saturating_mul(RETAINED_CONNECTIONS_PER_REFERENCE_SLOT))
    }

    /// Fix an exact capacity for tests that need to reach the bound quickly.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) const fn exact_for_test(capacity: usize) -> Self {
        Self(capacity)
    }

    /// The bound as a count of lifecycle records.
    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

/// One admitted generation together with its data-channel age, when known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetentionCandidate {
    attempt: PendingConnectionAttempt,
    /// `None` when liveness holds no record for this generation; such a peer
    /// has not proven an open data channel and is treated as within grace.
    connected_for_ms: Option<i64>,
}

impl RetentionCandidate {
    /// `Evictable(n, p, now)` restricted to the locally observable part of
    /// `Referenced`: topology references are decided here, storage placement
    /// is checked by the effect layer only for peers that pass this filter.
    fn is_evictable(&self, referenced: &BTreeSet<Did>) -> bool {
        !referenced.contains(&self.attempt.peer)
            && self
                .connected_for_ms
                .is_some_and(|age_ms| age_ms >= UNREFERENCED_CONNECTION_GRACE_MS)
    }
}

/// `EvictionOrder(A, R)`: the evictable candidates, oldest data channel first.
///
/// Post: every returned attempt is unreferenced by `referenced` and past
/// grace; ties keep the caller's order, which is DID order for registry
/// projections, so the plan is deterministic for one snapshot.
fn eviction_order(
    candidates: Vec<RetentionCandidate>,
    referenced: &BTreeSet<Did>,
) -> Vec<PendingConnectionAttempt> {
    let mut evictable = candidates
        .into_iter()
        .filter(|candidate| candidate.is_evictable(referenced))
        .collect::<Vec<_>>();
    evictable.sort_by_key(|candidate| Reverse(candidate.connected_for_ms));
    evictable
        .into_iter()
        .map(|candidate| candidate.attempt)
        .collect()
}

impl SwarmTransport {
    /// Snapshot the eviction plan under the lifecycle boundary.
    ///
    /// Topology references and connection ages are read under one lock so a
    /// concurrent admission or retirement cannot interleave with the plan.
    fn unreferenced_eviction_order(&self, now_ms: i64) -> Result<Vec<PendingConnectionAttempt>> {
        self.with_connection_lifecycle(|| {
            let referenced = self.dht.topology_state()?.referenced_peers();
            let admitted = self.peer_lifecycles()?.admitted_connections();
            let liveness = self.peer_liveness()?;
            let candidates = admitted
                .iter()
                .map(|attempt| RetentionCandidate {
                    attempt,
                    connected_for_ms: liveness.connected_for_ms(
                        attempt.peer,
                        attempt.generation,
                        now_ms,
                    ),
                })
                .collect();
            Ok(eviction_order(candidates, &referenced))
        })
    }

    /// Retire the oldest unreferenced admitted connection to free one slot.
    ///
    /// Pre: the lifecycle registry is full for a peer that owns no record.
    /// Post: `Ok(Some(peer))` iff one admitted generation that no local
    /// topology slot or storage placement references, and whose data channel
    /// has been open for at least [`UNREFERENCED_CONNECTION_GRACE_MS`], left
    /// the DHT and had its transport closed. Referenced and younger
    /// connections are preserved, so `Ok(None)` leaves the reservation to be
    /// rejected by the registry bound. Evidence superseded between the plan
    /// and the retirement is skipped in favour of the next candidate.
    pub(super) async fn evict_unreferenced_connection(&self, now_ms: i64) -> Result<Option<Did>> {
        for attempt in self.unreferenced_eviction_order(now_ms)? {
            if self
                .dht
                .peer_may_share_storage_responsibility(attempt.peer, self.storage_redundancy())
                .await?
            {
                continue;
            }
            if !self.disconnect_attempt(attempt).await? {
                continue;
            }
            tracing::info!(
                target: "rings_core::swarm::transport::retention",
                local = %self.dht.did,
                peer = %attempt.peer,
                generation = attempt.generation,
                grace_ms = UNREFERENCED_CONNECTION_GRACE_MS,
                "evicted unreferenced connection to admit a new peer"
            );
            return Ok(Some(attempt.peer));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn candidate(peer: Did, generation: u64, connected_for_ms: Option<i64>) -> RetentionCandidate {
        RetentionCandidate {
            attempt: PendingConnectionAttempt { peer, generation },
            connected_for_ms,
        }
    }

    #[test]
    fn test_capacity_covers_both_reference_directions_of_every_slot() {
        assert_eq!(
            ConnectionCapacity::for_successor_capacity(3).get(),
            (DEFAULT_FINGER_TABLE_SIZE + 3 + 1) * RETAINED_CONNECTIONS_PER_REFERENCE_SLOT
        );
    }

    #[test]
    fn test_eviction_order_keeps_referenced_young_and_unproven_peers() {
        let referenced_peer = SecretKey::random().address().into();
        let young = SecretKey::random().address().into();
        let unproven = SecretKey::random().address().into();
        let old = SecretKey::random().address().into();
        let older = SecretKey::random().address().into();
        let referenced = BTreeSet::from([referenced_peer]);

        let order = eviction_order(
            vec![
                candidate(
                    referenced_peer,
                    1,
                    Some(UNREFERENCED_CONNECTION_GRACE_MS * 4),
                ),
                candidate(young, 2, Some(UNREFERENCED_CONNECTION_GRACE_MS - 1)),
                candidate(unproven, 3, None),
                candidate(old, 4, Some(UNREFERENCED_CONNECTION_GRACE_MS)),
                candidate(older, 5, Some(UNREFERENCED_CONNECTION_GRACE_MS * 2)),
            ],
            &referenced,
        );

        assert_eq!(order, vec![
            PendingConnectionAttempt {
                peer: older,
                generation: 5
            },
            PendingConnectionAttempt {
                peer: old,
                generation: 4
            },
        ]);
    }

    #[test]
    fn test_eviction_order_is_stable_for_equal_ages() {
        let mut peers = (0..3)
            .map(|_| Did::from(SecretKey::random().address()))
            .collect::<Vec<_>>();
        peers.sort();
        let candidates = peers
            .iter()
            .enumerate()
            .map(|(index, peer)| {
                candidate(*peer, index as u64, Some(UNREFERENCED_CONNECTION_GRACE_MS))
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
