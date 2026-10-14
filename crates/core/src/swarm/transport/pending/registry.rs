use std::collections::BTreeMap;
use std::collections::BTreeSet;

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
pub(in crate::swarm::transport) enum PeerConnectionLifecycle {
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

impl PeerConnectionLifecycle {
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
    state: &'state mut PeerConnectionLifecycle,
    attempt: PendingConnectionAttempt,
}

impl AdmittingConnection<'_> {
    /// Apply `Admitting(attempt) -> Active(attempt)`.
    pub(in crate::swarm::transport) fn activate(self) {
        *self.state = PeerConnectionLifecycle::Active {
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
    #[cfg(test)]
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
    peers: BTreeMap<Did, PeerConnectionLifecycle>,
    /// The signaling state of every pending record (`Invariant (Signaling)`).
    handshakes: BTreeMap<Did, Handshake>,
    send_terminal: BTreeSet<PendingConnectionAttempt>,
}

impl ConnectionLifecycleRegistry {
    /// Create an empty registry under `bounds`.
    pub(in crate::swarm::transport) fn new(bounds: LifecycleBounds) -> Self {
        Self {
            bounds,
            next_generation: 0,
            peers: BTreeMap::new(),
            handshakes: BTreeMap::new(),
            send_terminal: BTreeSet::new(),
        }
    }

    /// Reserve a pending generation for `peer` with a test-default signaling state: an
    /// external offer still being created.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn reserve(
        &mut self,
        peer: Did,
        now_ms: i64,
    ) -> Result<PendingConnectionAttempt> {
        self.reserve_for(
            peer,
            now_ms,
            Handshake::offering(super::handshake::OfferOwner::External),
        )
    }

    /// Reserve a pending generation for `peer` whose signaling starts in `handshake`.
    pub(in crate::swarm::transport) fn reserve_for(
        &mut self,
        peer: Did,
        now_ms: i64,
        handshake: Handshake,
    ) -> Result<PendingConnectionAttempt> {
        match self.reservation_verdict(peer) {
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
        self.peers
            .insert(attempt.peer, PeerConnectionLifecycle::Pending {
                attempt,
                started_at_ms: now_ms,
            });
    }

    pub(in crate::swarm::transport) fn contains(&self, peer: Did) -> bool {
        self.peers.contains_key(&peer)
    }

    /// Decide `reserve(peer)` without mutating: duplicate peer first, then the
    /// handshake bound, then the total bound.
    pub(in crate::swarm::transport) fn reservation_verdict(&self, peer: Did) -> ReservationVerdict {
        if self.contains(peer) {
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

    pub(in crate::swarm::transport) fn state(&self, peer: Did) -> Option<PeerConnectionLifecycle> {
        self.peers.get(&peer).copied()
    }

    pub(in crate::swarm::transport) fn pending_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Pending { attempt, .. }) => Some(attempt),
            Some(PeerConnectionLifecycle::Admitting { .. })
            | Some(PeerConnectionLifecycle::Active { .. })
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn unadmitted_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Pending { attempt, .. })
            | Some(PeerConnectionLifecycle::Admitting { attempt, .. }) => Some(attempt),
            Some(PeerConnectionLifecycle::Active { .. }) | None => None,
        }
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(in crate::swarm::transport) fn admitting_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Admitting { attempt, .. }) => Some(attempt),
            Some(PeerConnectionLifecycle::Pending { .. })
            | Some(PeerConnectionLifecycle::Active { .. })
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn active_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Active { attempt }) => Some(attempt),
            Some(PeerConnectionLifecycle::Pending { .. })
            | Some(PeerConnectionLifecycle::Admitting { .. })
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
                    PeerConnectionLifecycle::Active { attempt }
                        if !self.send_terminal.contains(attempt) =>
                    {
                        Some((*peer, *attempt))
                    }
                    PeerConnectionLifecycle::Active { .. } => None,
                    PeerConnectionLifecycle::Pending { .. }
                    | PeerConnectionLifecycle::Admitting { .. } => None,
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
                    PeerConnectionLifecycle::Active { attempt } => Some((*peer, *attempt)),
                    PeerConnectionLifecycle::Pending { .. }
                    | PeerConnectionLifecycle::Admitting { .. } => None,
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
        let PeerConnectionLifecycle::Pending {
            attempt: current,
            started_at_ms,
        } = *state
        else {
            return false;
        };
        if current != attempt {
            return false;
        }
        *state = PeerConnectionLifecycle::Admitting {
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
            PeerConnectionLifecycle::Admitting {
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
            Some(PeerConnectionLifecycle::Pending {
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
                    PeerConnectionLifecycle::Pending {
                        attempt,
                        started_at_ms,
                    } => (attempt, started_at_ms, UnadmittedPhase::Pending),
                    PeerConnectionLifecycle::Admitting {
                        attempt,
                        started_at_ms,
                    } => (attempt, started_at_ms, UnadmittedPhase::Admitting),
                    PeerConnectionLifecycle::Active { .. } => return None,
                };
                let age_ms = now_ms.saturating_sub(*started_at_ms);
                (age_ms >= PENDING_CONNECTION_TIMEOUT_MS).then_some(ExpiredUnadmittedPeer {
                    attempt: *attempt,
                    age_ms,
                    phase,
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

    /// Apply `Offering(o, Creating) -> Offering(o, Sent(sdp, tx_id))`.
    pub(in crate::swarm::transport) fn mark_offer_sent(
        &mut self,
        attempt: PendingConnectionAttempt,
        sdp: String,
        tx_id: uuid::Uuid,
    ) -> bool {
        self.advance_handshake(attempt, |current| match current {
            Handshake::Offering {
                owner,
                phase: OfferPhase::Creating,
            } => Some(Handshake::Offering {
                owner: *owner,
                phase: OfferPhase::Sent { sdp, tx_id },
            }),
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Apply `Offering(o, Sent(_, t)) -> Offering(o, Answered)` for an answer echoing `t`
    /// (`tx_id = Some(t)`); an answer carried out of band (`tx_id = None`) also accepts an
    /// offer still `Creating`, whose owner holds it.
    pub(in crate::swarm::transport) fn mark_offer_answered(
        &mut self,
        attempt: PendingConnectionAttempt,
        tx_id: Option<uuid::Uuid>,
    ) -> bool {
        self.advance_handshake(attempt, |current| match current {
            Handshake::Offering {
                owner,
                phase: OfferPhase::Sent { tx_id: sent, .. },
            } if tx_id.is_none_or(|tx_id| tx_id == *sent) => Some(Handshake::Offering {
                owner: *owner,
                phase: OfferPhase::Answered,
            }),
            Handshake::Offering {
                owner,
                phase: OfferPhase::Creating,
            } if tx_id.is_none() => Some(Handshake::Offering {
                owner: *owner,
                phase: OfferPhase::Answered,
            }),
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// Apply `Answering(false) -> Answering(true)`.
    pub(in crate::swarm::transport) fn mark_reply_sent(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        self.advance_handshake(attempt, |current| match current {
            Handshake::Answering { reply_sent: false } => {
                Some(Handshake::Answering { reply_sent: true })
            }
            Handshake::Offering { .. } | Handshake::Answering { .. } => None,
        })
    }

    /// The pending generation of `peer` whose sent, unanswered offer left under `tx_id`: the
    /// only generation an answer echoing `tx_id` may be applied to.
    pub(in crate::swarm::transport) fn offer_sent_under(
        &self,
        peer: Did,
        tx_id: uuid::Uuid,
    ) -> Option<PendingConnectionAttempt> {
        let attempt = self.pending_attempt(peer)?;
        matches!(
            self.handshakes.get(&peer),
            Some(Handshake::Offering {
                phase: OfferPhase::Sent { tx_id: sent, .. },
                ..
            }) if *sent == tx_id
        )
        .then_some(attempt)
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
            Some(PeerConnectionLifecycle::Pending {
                attempt: current, ..
            }) => {
                current != attempt
                    && matches!(
                        self.handshakes.get(&attempt.peer),
                        Some(Handshake::Answering { .. })
                    )
            }
            Some(
                PeerConnectionLifecycle::Admitting {
                    attempt: current, ..
                }
                | PeerConnectionLifecycle::Active {
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
                    PeerConnectionLifecycle::Pending { .. }
                        | PeerConnectionLifecycle::Admitting { .. }
                )
            })
            .count()
    }
}
