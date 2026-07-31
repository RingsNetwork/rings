use std::collections::BTreeMap;

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
pub(in crate::swarm::transport) enum PeerConnectionLifecycle {
    Pending {
        attempt: PendingConnectionAttempt,
        started_at_ms: i64,
    },
    Admitting {
        attempt: PendingConnectionAttempt,
        started_at_ms: i64,
    },
    Active(PendingConnectionAttempt),
}

impl PeerConnectionLifecycle {
    pub(in crate::swarm::transport) const fn attempt(self) -> PendingConnectionAttempt {
        match self {
            Self::Pending { attempt, .. }
            | Self::Admitting { attempt, .. }
            | Self::Active(attempt) => attempt,
        }
    }
}

pub(in crate::swarm::transport) struct AdmittingConnection<'state> {
    state: &'state mut PeerConnectionLifecycle,
    attempt: PendingConnectionAttempt,
}

impl AdmittingConnection<'_> {
    pub(in crate::swarm::transport) fn activate(self) {
        *self.state = PeerConnectionLifecycle::Active(self.attempt);
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

/// Read-only projection of the currently admitted connection generations.
#[derive(Debug)]
pub(in crate::swarm::transport) struct ActiveConnectionSet {
    attempts: BTreeMap<Did, PendingConnectionAttempt>,
}

impl ActiveConnectionSet {
    pub(in crate::swarm::transport) fn contains(&self, peer: Did) -> bool {
        self.attempts.contains_key(&peer)
    }

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

/// Registry of mutually exclusive pending, admitting, and active generations.
///
/// Model: `State = Did ->?
/// (Pending(attempt, started_at) | Admitting(attempt, started_at) | Active(attempt))`.
/// Initial state is the empty map. The complete next-state relation is
/// `reserve | begin_admission | activate | remove_unadmitted | remove_active | expire`.
///
/// Invariant: every peer has at most one generation and one lifecycle phase.
/// Only `Active` belongs to the routable projection.
#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
pub(in crate::swarm::transport) struct ConnectionLifecycleRegistry<const MAX_PENDING: usize> {
    next_generation: u64,
    peers: BTreeMap<Did, PeerConnectionLifecycle>,
}

impl<const MAX_PENDING: usize> ConnectionLifecycleRegistry<MAX_PENDING> {
    pub(in crate::swarm::transport) fn new() -> Self {
        Self {
            next_generation: 0,
            peers: BTreeMap::new(),
        }
    }

    pub(in crate::swarm::transport) fn reserve(
        &mut self,
        peer: Did,
        now_ms: i64,
    ) -> Result<PendingConnectionAttempt> {
        if self.peers.contains_key(&peer) {
            return Err(Error::AlreadyConnected);
        }
        if self.pending_len() >= MAX_PENDING {
            return Err(Error::PendingConnectionCapacityExceeded {
                capacity: MAX_PENDING,
            });
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
            | Some(PeerConnectionLifecycle::Active(_))
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
            Some(PeerConnectionLifecycle::Active(_)) | None => None,
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
            | Some(PeerConnectionLifecycle::Active(_))
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn active_attempt(
        &self,
        peer: Did,
    ) -> Option<PendingConnectionAttempt> {
        match self.state(peer) {
            Some(PeerConnectionLifecycle::Active(attempt)) => Some(attempt),
            Some(PeerConnectionLifecycle::Pending { .. })
            | Some(PeerConnectionLifecycle::Admitting { .. })
            | None => None,
        }
    }

    pub(in crate::swarm::transport) fn active_connections(&self) -> ActiveConnectionSet {
        ActiveConnectionSet {
            attempts: self
                .peers
                .iter()
                .filter_map(|(peer, state)| match state {
                    PeerConnectionLifecycle::Active(attempt) => Some((*peer, *attempt)),
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

    /// Apply `Active(attempt) -> Absent`.
    pub(in crate::swarm::transport) fn remove_active(
        &mut self,
        attempt: PendingConnectionAttempt,
    ) -> bool {
        if self.active_attempt(attempt.peer) != Some(attempt) {
            return false;
        }
        self.peers.remove(&attempt.peer);
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
                    PeerConnectionLifecycle::Active(_) => return None,
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
