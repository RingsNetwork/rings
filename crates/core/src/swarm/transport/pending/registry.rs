use std::collections::BTreeMap;
use std::collections::BTreeSet;

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
    Active {
        attempt: PendingConnectionAttempt,
        /// Whether the admission was announced to the application. Set by `mark_announced`
        /// in the delivery turn that then starts `Connected`, and read by `retire_active_if`,
        /// the one production retirement, under the same lock; those two facts give, for
        /// every retired generation, `start(Connected) ⟺ start(PeerRetired)`.
        announced: bool,
    },
}

impl PeerConnectionLifecycle {
    pub(in crate::swarm::transport) const fn attempt(self) -> PendingConnectionAttempt {
        match self {
            Self::Pending { attempt, .. }
            | Self::Admitting { attempt, .. }
            | Self::Active { attempt, .. } => attempt,
        }
    }
}

/// Witness that an admitted connection record was retired under the lifecycle boundary,
/// carrying whether its admission had been announced. Only `retire_active_if` constructs one.
/// It is announced through `SwarmTransport::announce_retirement`, which delivers
/// [`SwarmEvent::PeerRetired`](crate::swarm::callback::SwarmEvent::PeerRetired) exactly when
/// the admission was; `retire_announced_if` is the single production path from a retirement
/// to its announcement.
#[derive(Debug)]
pub(in crate::swarm::transport) struct Retirement {
    announced_admission: bool,
}

impl Retirement {
    /// Whether the admission this retirement ends had been announced to the application.
    pub(in crate::swarm::transport) fn announced_admission(&self) -> bool {
        self.announced_admission
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
    /// Apply `Admitting(attempt) -> Active(attempt, unannounced)`.
    pub(in crate::swarm::transport) fn activate(self) {
        *self.state = PeerConnectionLifecycle::Active {
            attempt: self.attempt,
            announced: false,
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
/// Active(attempt, announced)), Terminal)`. Initial state is the empty map. The complete
/// next-state relation is `reserve | begin_admission | activate | mark_send_terminal |
/// mark_announced | remove_pending | remove_unadmitted | retire_active_if | expire`;
/// `remove_active` is the test-only unconditional form of `retire_active_if`.
///
/// Invariant: every peer has at most one generation and one lifecycle phase.
/// `Active` belongs to the admitted projection; send-terminal generations are
/// excluded from the routable projection until retirement removes them.
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
    send_terminal: BTreeSet<PendingConnectionAttempt>,
}

impl ConnectionLifecycleRegistry {
    /// Create an empty registry under `bounds`.
    pub(in crate::swarm::transport) fn new(bounds: LifecycleBounds) -> Self {
        Self {
            bounds,
            next_generation: 0,
            peers: BTreeMap::new(),
            send_terminal: BTreeSet::new(),
        }
    }

    pub(in crate::swarm::transport) fn reserve(
        &mut self,
        peer: Did,
        now_ms: i64,
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

        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(Error::PendingConnectionGenerationExhausted)?;
        let attempt = PendingConnectionAttempt {
            peer,
            generation: self.next_generation,
        };
        self.peers.insert(peer, PeerConnectionLifecycle::Pending {
            attempt,
            started_at_ms: now_ms,
        });
        Ok(attempt)
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
            Some(PeerConnectionLifecycle::Active { attempt, .. }) => Some(attempt),
            Some(PeerConnectionLifecycle::Pending { .. })
            | Some(PeerConnectionLifecycle::Admitting { .. })
            | None => None,
        }
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
        if self.active_attempt(attempt.peer) != Some(attempt) {
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
                    PeerConnectionLifecycle::Active { attempt, .. }
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
                    PeerConnectionLifecycle::Active { attempt, .. } => Some((*peer, *attempt)),
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

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
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
        self.peers.remove(&attempt.peer);
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
        self.peers.remove(&attempt.peer);
        true
    }

    /// Apply `Active(attempt, _) -> Active(attempt, announced)`; `false` when `attempt` does
    /// not own the active slot.
    pub(in crate::swarm::transport) fn mark_announced(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        match self.peers.get_mut(&attempt.peer) {
            Some(PeerConnectionLifecycle::Active {
                attempt: current,
                announced,
            }) if *current == attempt => {
                *announced = true;
                true
            }
            _ => false,
        }
    }

    /// The active generation of `peer` whose admission was announced, if any: the peer the
    /// application has been told about and will be told the retirement of.
    pub(in crate::swarm::transport) fn announced_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Active {
                attempt,
                announced: true,
            }) => Some(attempt),
            _ => None,
        }
    }

    /// Every active generation whose admission was announced, in DID order: the admitted
    /// projection the application has been told about, `{ a | peers(a.peer) = Active(a, true) }`.
    pub(in crate::swarm::transport) fn announced_attempts(
        &self,
    ) -> impl Iterator<Item = PendingConnectionAttempt> + '_ {
        self.peers.values().filter_map(|state| match *state {
            PeerConnectionLifecycle::Active {
                attempt,
                announced: true,
            } => Some(attempt),
            _ => None,
        })
    }

    /// The bounds the registry was created under.
    pub(in crate::swarm::transport) const fn bounds(&self) -> LifecycleBounds {
        self.bounds
    }

    /// Apply `Active(attempt, announced) -> Absent` iff `attempt` owns the active slot and
    /// `action`, given the active set, commits; the witness carries `announced`.
    ///
    /// Post: `Superseded` iff `attempt` was not the active generation; `Declined` iff `action`
    /// declined and no state changed; `Retired` iff `action` committed and the record is gone.
    pub(in crate::swarm::transport) fn retire_active_if<T>(
        &mut self,
        attempt: PendingConnectionAttempt,
        action: impl FnOnce(&ActiveConnectionSet) -> Result<Option<T>>,
    ) -> Result<RetirementOutcome<(T, Retirement)>> {
        let announced = match self.state(attempt.peer) {
            Some(PeerConnectionLifecycle::Active {
                attempt: current,
                announced,
            }) if current == attempt => announced,
            _ => return Ok(RetirementOutcome::Superseded),
        };
        let Some(value) = action(&self.active_connections())? else {
            return Ok(RetirementOutcome::Declined);
        };
        self.peers.remove(&attempt.peer);
        self.send_terminal.remove(&attempt);
        Ok(RetirementOutcome::Retired((value, Retirement {
            announced_admission: announced,
        })))
    }

    /// Apply `Active(attempt, _) -> Absent`; the test-only unconditional form of
    /// `retire_active_if`.
    #[cfg(test)]
    pub(in crate::swarm::transport) fn remove_active(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if self.active_attempt(attempt.peer) != Some(attempt) {
            return false;
        }
        self.peers.remove(&attempt.peer);
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
            self.peers.remove(&expired.attempt.peer);
        }
        expired
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
