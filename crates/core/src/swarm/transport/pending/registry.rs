use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bytes::Bytes;
use rings_transport::core::transport::MAX_HANDSHAKE_STEPS;

use super::handshake::Handshake;
use super::handshake::OfferPhase;
use super::PendingConnectionAttempt;
use super::PENDING_CONNECTION_TIMEOUT_MS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// One live logical connection state for a peer.
///
/// Absence is represented by the peer not appearing in
/// [`ConnectionLifecycleRegistry::peers`]. A present peer therefore has exactly
/// one state and cannot be pending, admitting, and active at the same time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(in crate::swarm::transport) enum PeerSlotLifecycle {
    Pending {
        attempt: PendingConnectionAttempt,
        started_at_ms: i64,
    },
    Admitting {
        attempt: PendingConnectionAttempt,
        started_at_ms: i64,
    },
    /// Admitted. The transport announces the admission to the application in the critical
    /// section that activates the record, and the retirement in the one that removes it, so for
    /// every generation `PeerAdmitted ⟺ PeerRetired` announced, in the order decided.
    Active { attempt: PendingConnectionAttempt },
}

impl PeerSlotLifecycle {
    pub(in crate::swarm::transport) const fn attempt(self) -> PendingConnectionAttempt {
        match self {
            Self::Pending { attempt, .. }
            | Self::Admitting { attempt, .. }
            | Self::Active { attempt } => attempt,
        }
    }
}

/// Outcome of a retirement decided under the lifecycle boundary: `Superseded` when the
/// generation no longer owned the active slot, `Declined` when the deciding action kept the
/// record, `Retired` with the action's value once the record is gone.
#[derive(Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum RetirementOutcome<T> {
    /// The generation no longer owned the active slot; nothing changed.
    Superseded,
    /// The action declined; no local state changed.
    Declined,
    /// The record was retired; carries the action's value.
    Retired(T),
}

impl<T> RetirementOutcome<T> {
    /// The retired value, if the record was retired.
    pub(in crate::swarm::transport) fn retired(self) -> Option<T> {
        match self {
            Self::Retired(value) => Some(value),
            Self::Superseded | Self::Declined => None,
        }
    }

    /// Whether the record was retired.
    pub(in crate::swarm::transport) fn is_retired(&self) -> bool {
        matches!(self, Self::Retired(_))
    }
}

pub(in crate::swarm::transport) struct AdmittingConnection<'state> {
    state: &'state mut PeerSlotLifecycle,
    attempt: PendingConnectionAttempt,
}

impl AdmittingConnection<'_> {
    /// Apply `Admitting(attempt) -> Active(attempt)`.
    pub(in crate::swarm::transport) fn activate(self) {
        *self.state = PeerSlotLifecycle::Active {
            attempt: self.attempt,
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum UnadmittedPhase {
    Pending,
    Admitting,
}

impl UnadmittedPhase {
    pub(in crate::swarm::transport) const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Admitting => "admitting",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) struct ExpiredUnadmittedPeer {
    pub(in crate::swarm::transport) attempt: PendingConnectionAttempt,
    pub(in crate::swarm::transport) age_ms: i64,
    pub(in crate::swarm::transport) phase: UnadmittedPhase,
    pub(in crate::swarm::transport) was_offering: bool,
}

/// Read-only projection of active generations still eligible for data-plane work.
#[derive(Debug)]
pub(in crate::swarm::transport) struct ActiveConnectionSet {
    attempts: BTreeMap<Did, PendingConnectionAttempt>,
}

impl ActiveConnectionSet {
    pub(in crate::swarm::transport) fn attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        self.attempts.get(&peer).copied()
    }

    pub(in crate::swarm::transport) fn iter(
        &self,
    ) -> impl Iterator<Item = PendingConnectionAttempt> + '_ {
        self.attempts.values().copied()
    }
}

/// Cardinality bounds over the lifecycle map.
///
/// Invariant: `pending <= total`, so a handshake slot always fits inside the
/// total and a pending-saturated registry is never mistaken for a full one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(in crate::swarm::transport) struct LifecycleBounds {
    pending: usize,
    total: usize,
}

impl LifecycleBounds {
    /// Bound `Pending ∪ Admitting` by `pending` and every phase by `total`,
    /// clamping the handshake share into the total.
    pub(in crate::swarm::transport) const fn new(pending: usize, total: usize) -> Self {
        Self {
            pending: if pending < total { pending } else { total },
            total,
        }
    }

    /// Maximum peers in `Pending ∪ Admitting`, the handshake working set.
    #[cfg(test)]
    pub(in crate::swarm::transport) const fn pending(self) -> usize {
        self.pending
    }

    /// Maximum peers in any phase.
    pub(in crate::swarm::transport) const fn total(self) -> usize {
        self.total
    }
}

/// Why `reserve` would refuse a peer, or that it would admit it.
///
/// One source for the admission rule: `reserve` and the eviction policy both
/// read it, so a reservation that cannot succeed for another reason never
/// triggers an eviction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum ReservationVerdict {
    /// The registry is closed (the close law of [`ConnectionLifecycleRegistry::close`]).
    Closed,
    /// The peer already owns a lifecycle record.
    AlreadyConnected,
    /// The handshake working set is saturated.
    PendingCapacityExceeded,
    /// Every record slot is occupied; retiring one would admit the peer.
    CapacityExceeded,
    /// The reservation would be admitted.
    Admissible,
}

impl ReservationVerdict {
    /// Whether retiring one admitted record is what stands between this peer
    /// and admission.
    pub(in crate::swarm::transport) const fn needs_eviction(self) -> bool {
        matches!(self, Self::CapacityExceeded)
    }
}

/// Registry of mutually exclusive pending, admitting, and active generations.
///
/// Model: `State = (Did ->? (Pending(attempt, started_at) | Admitting(attempt, started_at) |
/// Active(attempt)), Terminal)`. Initial state is the empty map. The complete next-state
/// relation is `reserve | begin_admission | activate | mark_send_terminal | remove_pending |
/// remove_unadmitted | retire_active_if | expire`;
/// `remove_active` is the test-only unconditional form of `retire_active_if`.
///
/// Invariant: every peer has at most one generation and one lifecycle phase.
/// `Active` belongs to the admitted projection; send-terminal generations are
/// excluded from the routable projection until retirement removes them.
///
/// Invariant (Signaling): `dom(handshakes) = { p | peers[p] = Pending(_) }`. A pending record
/// carries its signaling state ([`Handshake`]); `reserve_for` creates both and every transition
/// out of `Pending` drops both, so neither outlives the other.
///
/// Invariant (Closed): once `closed`, the map only shrinks: `reserve_for` refuses every peer.
///
/// Invariant: `|Pending ∪ Admitting| <= bounds.pending()` and `|State| <= bounds.total()`.
/// Preservation: `reserve` is the only transition that grows the map and it
/// checks both bounds; every other transition keeps or shrinks the map, so an
/// activation never exceeds the bound its reservation was admitted under.
///
/// Test builds derive structural equality and hashing (`next_generation` included, so equal
/// registries have equal futures): a model checker carries this registry itself as a state
/// component instead of a shadow of it.
#[derive(Debug)]
#[cfg_attr(test, derive(Clone, Eq, Hash, PartialEq))]
pub(in crate::swarm::transport) struct ConnectionLifecycleRegistry {
    bounds: LifecycleBounds,
    next_generation: u64,
    peers: BTreeMap<Did, PeerSlotLifecycle>,
    /// The signaling state of every pending record (`Invariant (Signaling)`).
    handshakes: BTreeMap<Did, Handshake>,
    send_terminal: BTreeSet<PendingConnectionAttempt>,
    /// Whether the registry is closed (`Invariant (Closed)`).
    closed: bool,
}

impl ConnectionLifecycleRegistry {
    /// The bounds the registry was created under.
    pub(in crate::swarm::transport) const fn bounds(&self) -> LifecycleBounds {
        self.bounds
    }

    /// Create an empty registry under `bounds`.
    pub(in crate::swarm::transport) fn new(bounds: LifecycleBounds) -> Self {
        Self {
            bounds,
            next_generation: 0,
            peers: BTreeMap::new(),
            handshakes: BTreeMap::new(),
            send_terminal: BTreeSet::new(),
            closed: false,
        }
    }

    /// Close the registry: it reserves no generation from now on, and the peers it holds a
    /// record of, in any phase, are returned for their owner to disconnect.
    ///
    /// ```text
    ///   close : Registry → (Registry, {Did})      close (r) = (r[closed ↦ true], dom r.peers)
    /// ```
    ///
    /// Close law: every generation is reserved before the close, and is among the returned
    /// peers, or is refused. Closing is idempotent; a second close returns the peers still held.
    pub(in crate::swarm::transport) fn close(&mut self) -> BTreeSet<Did> {
        self.closed = true;
        self.peers.keys().copied().collect()
    }

    /// Reserve a pending generation for `peer` with a test-default signaling state: an
    /// external offer still being created.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn reserve(
        &mut self,
        peer: Did,
        now_ms: i64,
    ) -> Result<PendingConnectionAttempt> {
        self.reserve_for(peer, now_ms, Handshake::TEST_DEFAULT)
    }

    /// Reserve a pending generation for `peer` whose signaling starts in `handshake`.
    pub(in crate::swarm::transport) fn reserve_for(
        &mut self,
        peer: Did,
        now_ms: i64,
        handshake: Handshake,
    ) -> Result<PendingConnectionAttempt> {
        match self.reservation_verdict(peer) {
            ReservationVerdict::Closed => return Err(Error::SwarmClosed),
            ReservationVerdict::AlreadyConnected => return Err(Error::AlreadyConnected),
            ReservationVerdict::PendingCapacityExceeded => {
                return Err(Error::PendingConnectionCapacityExceeded {
                    capacity: self.bounds.pending,
                });
            }
            ReservationVerdict::CapacityExceeded => {
                return Err(Error::ConnectionCapacityExceeded {
                    capacity: self.bounds.total,
                });
            }
            ReservationVerdict::Admissible => {}
        }

        let attempt = self.next_attempt(peer)?;
        self.insert_pending(attempt, now_ms, handshake);
        Ok(attempt)
    }

    /// Allocate the next generation for `peer`; generations are never reused.
    fn next_attempt(&mut self, peer: Did) -> Result<PendingConnectionAttempt> {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(Error::PendingConnectionGenerationExhausted)?;
        Ok(PendingConnectionAttempt {
            peer,
            generation: self.next_generation,
        })
    }

    /// Install `attempt` as its peer's pending record in `handshake`.
    fn insert_pending(
        &mut self,
        attempt: PendingConnectionAttempt,
        now_ms: i64,
        handshake: Handshake,
    ) {
        self.handshakes.insert(attempt.peer, handshake);
        self.peers.insert(attempt.peer, PeerSlotLifecycle::Pending {
            attempt,
            started_at_ms: now_ms,
        });
    }

    pub(in crate::swarm::transport) fn contains(&self, peer: Did) -> bool {
        self.peers.contains_key(&peer)
    }

    /// Decide `reserve(peer)` without mutating: a closed registry first, then a duplicate peer,
    /// then the handshake bound, then the total bound.
    pub(in crate::swarm::transport) fn reservation_verdict(&self, peer: Did) -> ReservationVerdict {
        if self.closed {
            ReservationVerdict::Closed
        } else if self.contains(peer) {
            ReservationVerdict::AlreadyConnected
        } else if self.pending_len() >= self.bounds.pending {
            ReservationVerdict::PendingCapacityExceeded
        } else if self.peers.len() >= self.bounds.total {
            ReservationVerdict::CapacityExceeded
        } else {
            ReservationVerdict::Admissible
        }
    }

    /// Replace the bounds of an empty registry.
    ///
    /// Pre: no lifecycle record exists, so no record was admitted under bounds
    /// the new ones would violate.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(in crate::swarm::transport) fn set_bounds_for_test(&mut self, bounds: LifecycleBounds) {
        debug_assert!(
            self.peers.is_empty(),
            "bounds may only change on an empty registry"
        );
        self.bounds = bounds;
    }

    pub(in crate::swarm::transport) fn state(&self, peer: Did) -> Option<PeerSlotLifecycle> {
        self.peers.get(&peer).copied()
    }

    pub(in crate::swarm::transport) fn pending_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerSlotLifecycle::Pending { attempt, .. }) => Some(attempt),
            Some(PeerSlotLifecycle::Admitting { .. })
            | Some(PeerSlotLifecycle::Active { .. })
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn unadmitted_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerSlotLifecycle::Pending { attempt, .. })
            | Some(PeerSlotLifecycle::Admitting { attempt, .. }) => Some(attempt),
            Some(PeerSlotLifecycle::Active { .. }) | None => None,
        }
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(in crate::swarm::transport) fn admitting_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerSlotLifecycle::Admitting { attempt, .. }) => Some(attempt),
            Some(PeerSlotLifecycle::Pending { .. })
            | Some(PeerSlotLifecycle::Active { .. })
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn active_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerSlotLifecycle::Active { attempt }) => Some(attempt),
            Some(PeerSlotLifecycle::Pending { .. })
            | Some(PeerSlotLifecycle::Admitting { .. })
            | None => None,
        }
    }

    /// Whether `attempt` is the generation that owns its peer's active slot.
    pub(in crate::swarm::transport) fn owns_active_slot(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        self.active_attempt(attempt.peer) == Some(attempt)
    }

    pub(in crate::swarm::transport) fn sendable_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        self.active_attempt(peer)
            .filter(|attempt| !self.send_terminal.contains(attempt))
    }

    /// Revoke new sends for an exact active generation without consuming the
    /// lifecycle record needed by asynchronous DHT and transport cleanup.
    pub(in crate::swarm::transport) fn mark_send_terminal(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if !self.owns_active_slot(attempt) {
            return false;
        }
        self.send_terminal.insert(attempt);
        true
    }

    pub(in crate::swarm::transport) fn is_send_terminal(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        self.send_terminal.contains(&attempt)
    }

    pub(in crate::swarm::transport) fn active_connections(&self) -> ActiveConnectionSet {
        ActiveConnectionSet {
            attempts: self
                .peers
                .iter()
                .filter_map(|(peer, state)| match state {
                    PeerSlotLifecycle::Active { attempt }
                        if !self.send_terminal.contains(attempt) =>
                    {
                        Some((*peer, *attempt))
                    }
                    PeerSlotLifecycle::Active { .. } => None,
                    PeerSlotLifecycle::Pending { .. } | PeerSlotLifecycle::Admitting { .. } => None,
                })
                .collect(),
        }
    }

    pub(in crate::swarm::transport) fn admitted_connections(&self) -> ActiveConnectionSet {
        ActiveConnectionSet {
            attempts: self
                .peers
                .iter()
                .filter_map(|(peer, state)| match state {
                    PeerSlotLifecycle::Active { attempt } => Some((*peer, *attempt)),
                    PeerSlotLifecycle::Pending { .. } | PeerSlotLifecycle::Admitting { .. } => None,
                })
                .collect(),
        }
    }

    /// Apply `Pending(attempt) -> Admitting(attempt)`.
    pub(in crate::swarm::transport) fn begin_admission(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        let Some(state) = self.peers.get_mut(&attempt.peer) else {
            return false;
        };
        let PeerSlotLifecycle::Pending {
            attempt: current,
            started_at_ms,
        } = *state
        else {
            return false;
        };
        if current != attempt {
            return false;
        }
        *state = PeerSlotLifecycle::Admitting {
            attempt,
            started_at_ms,
        };
        self.handshakes.remove(&attempt.peer);
        true
    }

    pub(in crate::swarm::transport) fn admitting_connection(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> Option<AdmittingConnection<'_>> {
        let state = self.peers.get_mut(&attempt.peer)?;
        if !matches!(
            *state,
            PeerSlotLifecycle::Admitting {
                attempt: current,
                ..
            } if current == attempt
        ) {
            return None;
        }
        Some(AdmittingConnection { state, attempt })
    }

    #[cfg(test)]
    pub(in crate::swarm::transport) fn activate_for_test(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if !self.begin_admission(attempt) {
            return false;
        }
        let Some(admitting) = self.admitting_connection(attempt) else {
            return false;
        };
        admitting.activate();
        true
    }

    /// Apply `Pending(attempt, observed) -> Pending(answering, Answering)` iff `attempt`'s
    /// signaling state is still `observed`, returning the answering generation: the yield of an
    /// offer to a crossing one.
    ///
    /// Law (Atomic yield): the yielded generation is replaced by the peer's answering
    /// generation in one transition, so the slot is never vacant between them; whoever reads
    /// the slot after the yield finds the peer's handshake (`superseded_by_peer`). A yield
    /// decided on one state never applies to a later one (`Law (Decided state)` of the
    /// handshake). One pending record replaces another, so no bound is consulted.
    pub(in crate::swarm::transport) fn replace_pending_in(
        &mut self,
        attempt: PendingConnectionAttempt,
        observed: &Handshake,
        now_ms: i64,
    ) -> Result<Option<PendingConnectionAttempt>> {
        if self.handshake(attempt) != Some(observed) {
            return Ok(None);
        }
        let answering = self.next_attempt(attempt.peer)?;
        self.insert_pending(answering, now_ms, Handshake::answering());
        Ok(Some(answering))
    }

    /// Apply `Pending(attempt) -> Absent`.
    pub(in crate::swarm::transport) fn remove_pending(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if !matches!(
            self.state(attempt.peer),
            Some(PeerSlotLifecycle::Pending {
                attempt: current,
                ..
            }) if current == attempt
        ) {
            return false;
        }
        self.remove_record(attempt.peer);
        true
    }

    /// Apply `Pending(attempt) | Admitting(attempt) -> Absent`.
    pub(in crate::swarm::transport) fn remove_unadmitted(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if self.unadmitted_attempt(attempt.peer) != Some(attempt) {
            return false;
        }
        self.remove_record(attempt.peer);
        true
    }

    /// Apply `Active(attempt) -> Absent` iff `attempt` owns the active slot and `action`, given
    /// the active set, commits.
    ///
    /// Post: `Superseded` iff `attempt` was not the active generation; `Declined` iff `action`
    /// declined and no state changed; `Retired` iff `action` committed and the record is gone.
    pub(in crate::swarm::transport) fn retire_active_if<T>(
        &mut self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<Option<T>>,
    ) -> Result<RetirementOutcome<T>> {
        if !self.owns_active_slot(attempt) {
            return Ok(RetirementOutcome::Superseded);
        }
        let Some(value) = action(&self.active_connections())? else {
            return Ok(RetirementOutcome::Declined);
        };
        self.remove_record(attempt.peer);
        self.send_terminal.remove(&attempt);
        Ok(RetirementOutcome::Retired(value))
    }

    /// Apply `Active(attempt) -> Absent`; the test-only unconditional form of
    /// `retire_active_if`.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn remove_active(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if !self.owns_active_slot(attempt) {
            return false;
        }
        self.remove_record(attempt.peer);
        self.send_terminal.remove(&attempt);
        true
    }

    #[cfg(test)]
    pub(in crate::swarm::transport) fn set_next_generation_for_test(
        &mut self,
        next_generation: u64,
    ) {
        self.next_generation = next_generation;
    }

    pub(in crate::swarm::transport) fn expire(
        &mut self,
        now_ms: i64,
    ) -> Vec<ExpiredUnadmittedPeer> {
        let expired = self
            .peers
            .values()
            .filter_map(|state| {
                let (attempt, started_at_ms, phase) = match state {
                    PeerSlotLifecycle::Pending {
                        attempt,
                        started_at_ms,
                    } => (attempt, started_at_ms, UnadmittedPhase::Pending),
                    PeerSlotLifecycle::Admitting {
                        attempt,
                        started_at_ms,
                    } => (attempt, started_at_ms, UnadmittedPhase::Admitting),
                    PeerSlotLifecycle::Active { .. } => return None,
                };
                let age_ms = now_ms.saturating_sub(*started_at_ms);
                let was_offering =
                    matches!(self.handshake(*attempt), Some(Handshake::Offering { .. }));
                (age_ms >= PENDING_CONNECTION_TIMEOUT_MS).then_some(ExpiredUnadmittedPeer {
                    attempt: *attempt,
                    age_ms,
                    phase,
                    was_offering,
                })
            })
            .collect::<Vec<_>>();
        for expired in &expired {
            self.remove_record(expired.attempt.peer);
        }
        expired
    }

    /// Remove `peer`'s record together with its signaling state (`Invariant (Signaling)`).
    fn remove_record(&mut self, peer: Did) {
        self.peers.remove(&peer);
        self.handshakes.remove(&peer);
    }

    /// The signaling state of `attempt`, iff it is `peer`'s pending generation.
    pub(in crate::swarm::transport) fn handshake(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Option<&Handshake> {
        (self.pending_attempt(attempt.peer) == Some(attempt))
            .then(|| self.handshakes.get(&attempt.peer))
            .flatten()
    }

    /// Apply `step` to the signaling state of `attempt` iff it is the pending generation and
    /// `step` accepts the state; returns whether it did.
    fn advance_handshake(
        &mut self,
        attempt: PendingConnectionAttempt,
        step: impl FnOnce(&Handshake) -> Option<Handshake>,
    ) -> bool {
        if self.pending_attempt(attempt.peer) != Some(attempt) {
            return false;
        }
        let Some(current) = self.handshakes.get_mut(&attempt.peer) else {
            return false;
        };
        match step(current) {
            Some(next) => {
                *current = next;
                true
            }
            None => false,
        }
    }

    /// Apply `Offering(o, Creating) -> Offering(o, Sent(0, final_step, bytes, tx_id, sid))` for
    /// step `0` (including 1-step `final_step == true`, which stays in `Sent(0)` so crossing
    /// 1-step offers resolve via `Law (Tie-break)`), or advance a claimed multi-round continuation
    /// `Offering(o, ClaimedReply(2k, sid)) -> Offering(o, Sent(2k, false, ..))` / `Answered`.
    pub(in crate::swarm::transport) fn mark_offer_sent(
        &mut self,
        attempt: PendingConnectionAttempt,
        step_index: u8,
        final_step: bool,
        handshake: Bytes,
        tx_id: uuid::Uuid,
        session_id: Option<[u8; 16]>,
    ) -> bool {
        if step_index >= MAX_HANDSHAKE_STEPS || !step_index.is_multiple_of(2) {
            return false;
        }
        self.advance_handshake(attempt, |current| match current {
            Handshake::Offering {
                owner,
                phase: OfferPhase::Creating,
            } if step_index == 0 => Some(Handshake::Offering {
                owner: *owner,
                phase: OfferPhase::Sent {
                    step_index: 0,
                    final_step,
                    handshake,
                    tx_id,
                    session_id,
                },
            }),
            Handshake::Offering {
                owner,
                phase:
                    OfferPhase::ClaimedReply {
                        next_step_index,
                        session_id: expected_sid,
                    },
            } if step_index == *next_step_index && session_id == Some(*expected_sid) => {
                if final_step {
                    Some(Handshake::Offering {
                        owner: *owner,
                        phase: OfferPhase::Answered,
                    })
                } else {
                    Some(Handshake::Offering {
                        owner: *owner,
                        phase: OfferPhase::Sent {
                            step_index,
                            final_step: false,
                            handshake,
                            tx_id,
                            session_id,
                        },
                    })
                }
            }
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Apply `Offering(o, Creating) -> Offering(o, Answered)` when a 0-step initiator completes
    /// locally with no outbound signaling message.
    pub(in crate::swarm::transport) fn mark_zero_step_completed(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        self.advance_handshake(attempt, |current| match current {
            Handshake::Offering {
                owner,
                phase: OfferPhase::Creating,
            } => Some(Handshake::Offering {
                owner: *owner,
                phase: OfferPhase::Answered,
            }),
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Atomically validate and claim an inbound `ConnectNodeReport` (`report_step_index`,
    /// `report_final`, `report_session_id`) on `attempt`:
    /// - Requires `Handshake::Offering` in `OfferPhase::Sent { final_step: false, .. }` (never
    ///   matches `Answering`, `ClaimedReply`, `Answered`, or a 1-step `final_step: true` offer);
    /// - Requires `tx_id` (when `Some`) to match the sent transaction, `report_step_index ==
    ///   sent_step + 1`, and any established `session_id` to match;
    /// - Transitions to `OfferPhase::Answered` when `report_final` is `true`, or to
    ///   `OfferPhase::ClaimedReply { next_step_index: report_step_index + 1, session_id }` when
    ///   `report_final` is `false`.
    pub(in crate::swarm::transport) fn claim_offer_reply(
        &mut self,
        attempt: PendingConnectionAttempt,
        tx_id: Option<uuid::Uuid>,
        report_step_index: u8,
        report_final: bool,
        report_session_id: Option<[u8; 16]>,
    ) -> bool {
        self.advance_handshake(attempt, |current| match current {
            Handshake::Offering {
                owner,
                phase:
                    OfferPhase::Sent {
                        step_index,
                        final_step: false,
                        tx_id: sent,
                        session_id: sent_sid,
                        ..
                    },
            } if tx_id.is_none_or(|expected_tx| expected_tx == *sent)
                && report_step_index == step_index.saturating_add(1)
                && sent_sid.is_none_or(|sid| report_session_id == Some(sid)) =>
            {
                if report_final {
                    Some(Handshake::Offering {
                        owner: *owner,
                        phase: OfferPhase::Answered,
                    })
                } else {
                    let sid = report_session_id?;
                    let next_step_index = report_step_index.saturating_add(1);
                    if next_step_index >= MAX_HANDSHAKE_STEPS {
                        return None;
                    }
                    Some(Handshake::Offering {
                        owner: *owner,
                        phase: OfferPhase::ClaimedReply {
                            next_step_index,
                            session_id: sid,
                        },
                    })
                }
            }
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Record that responder generation `attempt` sent a non-final odd step and now awaits even
    /// initiator continuation `expected_step` under `session_id`.
    pub(in crate::swarm::transport) fn set_answering_awaiting_step(
        &mut self,
        attempt: PendingConnectionAttempt,
        expected_step: u8,
        session_id: [u8; 16],
    ) -> bool {
        if expected_step == 0
            || !expected_step.is_multiple_of(2)
            || expected_step >= MAX_HANDSHAKE_STEPS
        {
            return false;
        }
        self.advance_handshake(attempt, |current| match current {
            Handshake::Answering {
                awaiting_step: None,
            } => Some(Handshake::Answering {
                awaiting_step: Some((expected_step, session_id)),
            }),
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Atomically claim the responder generation for `peer` awaiting even continuation step
    /// `step_index` with `session_id`, clearing `awaiting_step` so duplicate continuations cannot
    /// race.
    pub(in crate::swarm::transport) fn claim_answering_continuation(
        &mut self,
        peer: Did,
        step_index: u8,
        session_id: [u8; 16],
    ) -> Option<PendingConnectionAttempt> {
        let attempt = self.pending_attempt(peer)?;
        let claimed = self.advance_handshake(attempt, |current| match current {
            Handshake::Answering {
                awaiting_step: Some((expected_step, expected_sid)),
            } if *expected_step == step_index && *expected_sid == session_id => {
                Some(Handshake::Answering {
                    awaiting_step: None,
                })
            }
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        });
        claimed.then_some(attempt)
    }

    /// The pending generation of `peer` (along with the outbound `step_index` and optional
    /// `session_id`) whose sent, unanswered step left under `tx_id`: the only generation a
    /// `ConnectNodeReport` echoing `tx_id` may be applied to.
    pub(in crate::swarm::transport) fn offer_sent_under(
        &self,
        peer: Did,
        tx_id: uuid::Uuid,
    ) -> Option<(PendingConnectionAttempt, u8, Option<[u8; 16]>)> {
        let attempt = self.pending_attempt(peer)?;
        match self.handshakes.get(&peer) {
            Some(Handshake::Offering {
                phase:
                    OfferPhase::Sent {
                        step_index,
                        final_step: false,
                        tx_id: sent,
                        session_id,
                        ..
                    },
                ..
            }) if *sent == tx_id => Some((attempt, *step_index, *session_id)),
            _ => None,
        }
    }

    /// Whether another handshake took `attempt`'s slot: the slot holds another generation
    /// that answers the peer's offer, or one already past the handshake. The latter is most
    /// often the peer's handshake, and may be a later one of this node's own after `attempt`
    /// was released; either way an edge is being established, which is what a caller treating
    /// supersession as satisfied relies on.
    pub(in crate::swarm::transport) fn superseded_by_peer(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        match self.state(attempt.peer) {
            Some(PeerSlotLifecycle::Pending {
                attempt: current, ..
            }) => {
                current != attempt
                    && matches!(
                        self.handshakes.get(&attempt.peer),
                        Some(Handshake::Answering { .. })
                    )
            }
            Some(
                PeerSlotLifecycle::Admitting {
                    attempt: current, ..
                }
                | PeerSlotLifecycle::Active {
                    attempt: current, ..
                },
            ) => current != attempt,
            None => false,
        }
    }

    /// Count all incomplete admissions against the bounded handshake capacity.
    pub(in crate::swarm::transport) fn pending_len(&self) -> usize {
        self.peers
            .values()
            .filter(|state| {
                matches!(
                    state,
                    PeerSlotLifecycle::Pending { .. } | PeerSlotLifecycle::Admitting { .. }
                )
            })
            .count()
    }
}
