//! Overlay liveness of admitted peers, judged by probes alone (L3).
//!
//! Each admitted generation carries a [`ProbeClock`], a pure state machine:
//!
//! ```text
//!   Idle(last) ──issue(p, t)  [t − last ≥ PERIOD]──▶ Outstanding(p, t)
//!   Outstanding(p, t) ──answer(p)─────────────────▶ Idle(t)
//!   Outstanding(p, t) ──now − t ≥ TIMEOUT─────────▶ expired: the peer is evicted
//! ```
//!
//! Laws:
//! - **One outstanding.** `issue` is refused while a probe is outstanding, so a later probe
//!   never makes an earlier answer unacceptable (#913 R9 H1).
//! - **Deadline from issue.** The deadline counts from the instant the probe is issued; nothing
//!   extends it, and nothing waits for the probe to leave the queue (#913 R7 H1).
//! - **Answer only.** Only the answer to the outstanding probe clears it; a peer's other inbound
//!   traffic proves only that it sends, not that it takes this end's control traffic (#913 R8
//!   M1). Inbound traffic is recorded for retention alone.
//! - **Credit-blind.** No flow-control state is read. A peer that withholds its control lane's
//!   credit keeps the probe in its own queue, so the probe goes unanswered and the peer is
//!   evicted by the ordinary rule; a peer slow on a data lane answers on lane 0 and is kept
//!   (#913 R6 H1).
//! - **Probe slot.** An issued probe is witnessed by an [`IssuedProbe`], which exists only
//!   while its probe is outstanding; the probe's transfer takes no shared outbound capacity, as
//!   one-outstanding bounds it, so other traffic toward a peer cannot refuse the probe that
//!   judges it (#913 R8 M2), and it waits for credit until the answer deadline itself (#913 R10
//!   M5).
//! - **Unsent is resent, never withdrawn.** A probe that did not reach the queue (no ready
//!   connection, or a step cut short) stays outstanding with its deadline and is sent again, the
//!   same probe, on the next pass: its record keeps the signed payload until the probe reaches
//!   the queue or is answered. A peer that keeps its connection unready at each pass gains
//!   nothing: its deadline runs from the first issue (#913 R10 M7).

use std::collections::BTreeMap;
use std::sync::MutexGuard;

use super::pending::ActiveConnectionSet;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessagePayload;
use crate::message::ProbeRequest;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::swarm::transport::SwarmTransport;
use crate::utils::get_epoch_ms_i64;

/// Interval between two probes of one peer, counted from the earlier probe's issue.
pub(crate) const PEER_LIVENESS_PERIOD_MS: i64 = 15_000;
/// Deadline of a probe's answer, counted from its issue, after which the peer is evicted.
pub(crate) const PEER_LIVENESS_TIMEOUT_MS: i64 = 45_000;

/// The identity of one probe: the transaction it is sent under and its challenge.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProbeIdentity {
    /// Transaction of the probe's payload; its answer is a report under it.
    pub(crate) tx_id: uuid::Uuid,
    /// Challenge the answer must carry.
    pub(crate) request: ProbeRequest,
}

/// The probe clock of one admitted generation; see the module laws.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProbeClock {
    /// When the latest probe was issued, or the generation was admitted before any probe.
    last_issued_ms: i64,
    /// The outstanding probe and its issue instant.
    outstanding: Option<(ProbeIdentity, i64)>,
}

impl ProbeClock {
    /// The clock of a generation admitted at `admitted_ms`: its first probe is due a period
    /// later.
    pub(crate) const fn admitted_at(admitted_ms: i64) -> Self {
        Self {
            last_issued_ms: admitted_ms,
            outstanding: None,
        }
    }

    /// Whether a probe is due at `now_ms`: none is outstanding and a period has passed since
    /// the latest issue.
    pub(crate) fn due(&self, now_ms: i64) -> bool {
        self.outstanding.is_none()
            && now_ms.saturating_sub(self.last_issued_ms) >= PEER_LIVENESS_PERIOD_MS
    }

    /// Issue `probe` at `now_ms`. Refused (`false`) while another probe is outstanding.
    pub(crate) fn issue(&mut self, probe: ProbeIdentity, now_ms: i64) -> bool {
        if self.outstanding.is_some() {
            return false;
        }
        self.outstanding = Some((probe, now_ms));
        self.last_issued_ms = now_ms;
        true
    }

    /// How long the outstanding probe has left before its deadline, at `now_ms`.
    pub(crate) fn remaining_ms(&self, now_ms: i64) -> Option<i64> {
        self.outstanding_since_ms().map(|issued_ms| {
            issued_ms
                .saturating_add(PEER_LIVENESS_TIMEOUT_MS)
                .saturating_sub(now_ms)
                .max(0)
        })
    }

    /// Clear the outstanding probe on its answer. Returns whether `probe` was outstanding.
    pub(crate) fn answer(&mut self, probe: ProbeIdentity) -> bool {
        if self.outstanding.map(|(outstanding, _)| outstanding) != Some(probe) {
            return false;
        }
        self.outstanding = None;
        true
    }

    /// When the outstanding probe was issued, if one is.
    pub(crate) fn outstanding_since_ms(&self) -> Option<i64> {
        self.outstanding.map(|(_, issued_ms)| issued_ms)
    }

    /// The expiry evidence at `now_ms`: the outstanding probe is `TIMEOUT` old.
    pub(crate) fn expiry(&self, now_ms: i64) -> Option<PeerLivenessExpiry> {
        let unanswered_for_ms = now_ms.saturating_sub(self.outstanding_since_ms()?);
        (unanswered_for_ms >= PEER_LIVENESS_TIMEOUT_MS).then_some(PeerLivenessExpiry {
            unanswered_for_ms,
            timeout_ms: PEER_LIVENESS_TIMEOUT_MS,
        })
    }
}

/// A probe the clock of its generation issued, and the witness that it is outstanding: built
/// only here, by [`SwarmTransport::issue_liveness_probe`] and its resend, which is what lets its
/// transfer bypass the shared outbound capacity. Its fields are private, so no other code can
/// forge one.
pub(crate) struct IssuedProbe {
    attempt: PendingConnectionAttempt,
    identity: ProbeIdentity,
    payload: MessagePayload,
    /// How long the transfer may wait for credit: what is left of the answer deadline.
    deadline: std::time::Duration,
}

impl IssuedProbe {
    /// The generation the probe judges.
    pub(crate) fn attempt(&self) -> PendingConnectionAttempt {
        self.attempt
    }

    /// The probe's identity.
    pub(crate) fn identity(&self) -> ProbeIdentity {
        self.identity
    }

    /// The peer, payload and transfer deadline the egress sends.
    pub(super) fn into_send(self) -> (Did, MessagePayload, std::time::Duration) {
        (self.attempt.peer, self.payload, self.deadline)
    }
}

/// One step of a probe pass.
pub(crate) enum ProbeWork {
    /// Issue a fresh probe of this generation.
    Issue(PendingConnectionAttempt),
    /// Send again an outstanding probe that has not reached the queue (boxed: it carries the
    /// signed payload).
    Resend(Box<IssuedProbe>),
}

/// What is left of `clock`'s outstanding probe deadline at `now_ms`, as a transfer deadline.
fn remaining(clock: &ProbeClock, now_ms: i64) -> std::time::Duration {
    let remaining_ms = clock.remaining_ms(now_ms).unwrap_or(0);
    std::time::Duration::from_millis(u64::try_from(remaining_ms).unwrap_or(0))
}

/// One admitted generation's liveness record.
#[derive(Clone, Debug)]
struct PeerLiveness {
    generation: u64,
    connected_at_ms: i64,
    /// The latest authenticated inbound payload, read by retention, never by liveness.
    last_inbound_ms: i64,
    clock: ProbeClock,
    /// The signed payload of the outstanding probe while it has not reached the queue, to be
    /// sent again. Invariant: `unsent.is_some() ⟹ clock` has a probe outstanding.
    unsent: Option<MessagePayload>,
}

impl PeerLiveness {
    /// The liveness of `generation`, admitted at `now_ms`: its clock starts, nothing is
    /// outstanding.
    fn new(generation: u64, now_ms: i64) -> Self {
        Self {
            generation,
            connected_at_ms: now_ms,
            last_inbound_ms: now_ms,
            clock: ProbeClock::admitted_at(now_ms),
            unsent: None,
        }
    }

    /// Clear the outstanding probe on its answer, and its unsent payload with it. Returns
    /// whether `probe` was outstanding.
    fn answer(&mut self, probe: ProbeIdentity) -> bool {
        let answered = self.clock.answer(probe);
        if answered {
            self.unsent = None;
        }
        answered
    }
}

/// Bounded-by-active-peers overlay liveness state.
///
/// Invariant: for every `(did, state)` in this map, `state.generation` is the
/// active transport generation that admitted `did`. A new connection generation
/// resets liveness, so late observations from an old connection cannot prove the
/// new connection live.
pub(super) struct PeerLivenessMap {
    peers: BTreeMap<Did, PeerLiveness>,
}

impl PeerLivenessMap {
    pub(super) fn new() -> Self {
        Self {
            peers: BTreeMap::new(),
        }
    }

    fn mark_connected(&mut self, peer: Did, generation: u64, now_ms: i64) {
        self.peers
            .insert(peer, PeerLiveness::new(generation, now_ms));
    }

    fn mark_inbound(&mut self, peer: Did, generation: u64, now_ms: i64) {
        match self.peers.get_mut(&peer) {
            Some(liveness) if liveness.generation == generation => {
                liveness.last_inbound_ms = now_ms;
            }
            _ => self.mark_connected(peer, generation, now_ms),
        }
    }

    pub(super) fn remove(&mut self, peer: Did) {
        self.peers.remove(&peer);
    }

    /// The record of `peer`'s `generation`, if it is the one recorded.
    fn generation(&self, peer: Did, generation: u64) -> Option<&PeerLiveness> {
        self.peers
            .get(&peer)
            .filter(|liveness| liveness.generation == generation)
    }

    /// [`Self::generation`], mutably.
    fn generation_mut(&mut self, peer: Did, generation: u64) -> Option<&mut PeerLiveness> {
        self.peers
            .get_mut(&peer)
            .filter(|liveness| liveness.generation == generation)
    }

    fn retain_active(&mut self, active: &ActiveConnectionSet) {
        self.peers.retain(|peer, liveness| {
            active
                .attempt(*peer)
                .is_some_and(|attempt| attempt.generation == liveness.generation)
        });
    }

    /// What the probe pass must do at `now_ms`: issue a probe of every active generation that
    /// is due one, and send again every outstanding probe that has not reached the queue.
    fn probe_work(&mut self, active: &ActiveConnectionSet, now_ms: i64) -> Vec<ProbeWork> {
        self.retain_active(active);
        for attempt in active.iter() {
            self.peers
                .entry(attempt.peer)
                .or_insert_with(|| PeerLiveness::new(attempt.generation, now_ms));
        }
        active
            .iter()
            .filter_map(|attempt| {
                let liveness = self.peers.get(&attempt.peer)?;
                if liveness.clock.due(now_ms) {
                    return Some(ProbeWork::Issue(attempt));
                }
                let (Some((identity, ..)), Some(payload)) =
                    (liveness.clock.outstanding, liveness.unsent.clone())
                else {
                    return None;
                };
                Some(ProbeWork::Resend(Box::new(IssuedProbe {
                    attempt,
                    identity,
                    payload,
                    deadline: remaining(&liveness.clock, now_ms),
                })))
            })
            .collect()
    }

    pub(super) fn connected_for_ms(&self, peer: Did, generation: u64, now_ms: i64) -> Option<i64> {
        self.generation(peer, generation)
            .map(|liveness| now_ms.saturating_sub(liveness.connected_at_ms))
    }

    /// Time since the last authenticated inbound payload on this generation.
    pub(super) fn idle_for_ms(&self, peer: Did, generation: u64, now_ms: i64) -> Option<i64> {
        self.generation(peer, generation)
            .map(|liveness| now_ms.saturating_sub(liveness.last_inbound_ms))
    }
}

/// Peer liveness expiry evidence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerLivenessExpiry {
    /// How long the probe has been unanswered.
    pub(crate) unanswered_for_ms: i64,
    /// Configured timeout for unanswered probes.
    pub(crate) timeout_ms: i64,
}

/// What one liveness observation records.
#[derive(Clone, Copy)]
enum PeerLivenessObservation {
    /// The generation was admitted: its clock starts.
    Connected,
    /// An authenticated inbound payload arrived; retention reads it, liveness does not.
    Inbound,
}

impl PeerLivenessObservation {
    /// Record this observation of `peer`'s `generation` at `now_ms`.
    fn apply(self, liveness: &mut PeerLivenessMap, peer: Did, generation: u64, now_ms: i64) {
        match self {
            Self::Connected => liveness.mark_connected(peer, generation, now_ms),
            Self::Inbound => liveness.mark_inbound(peer, generation, now_ms),
        }
    }
}

impl SwarmTransport {
    pub(super) fn peer_liveness(&self) -> Result<MutexGuard<'_, PeerLivenessMap>> {
        self.peer_liveness
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn observe_peer_liveness(
        &self,
        attempt: PendingConnectionAttempt,
        observation: PeerLivenessObservation,
        before_update: impl FnOnce(),
    ) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        self.with_active_slot(attempt, || {
            before_update();
            let mut liveness = self.peer_liveness()?;
            observation.apply(&mut liveness, attempt.peer, attempt.generation, now_ms);
            Ok(())
        })
        .map(|_| ())
    }

    pub(crate) fn mark_peer_liveness_connected(&self, attempt: PendingConnectionAttempt) {
        if let Err(error) =
            self.observe_peer_liveness(attempt, PeerLivenessObservation::Connected, || {})
        {
            tracing::warn!(
                "failed to mark liveness for connected peer {} generation {}: {error}",
                attempt.peer,
                attempt.generation
            );
        }
    }

    pub(crate) fn mark_peer_liveness_inbound(&self, attempt: PendingConnectionAttempt) {
        if let Err(error) =
            self.observe_peer_liveness(attempt, PeerLivenessObservation::Inbound, || {})
        {
            tracing::warn!(
                "failed to mark liveness for inbound peer {} generation {}: {error}",
                attempt.peer,
                attempt.generation
            );
        }
    }

    /// What the probe pass must do at `now_ms`; see [`ProbeWork`].
    pub(crate) fn liveness_probe_work(&self, now_ms: i64) -> Result<Vec<ProbeWork>> {
        self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            Ok(self.peer_liveness()?.probe_work(&active, now_ms))
        })
    }

    /// Issue `payload`, carrying `request`, as `attempt`'s probe at `now_ms`: its deadline
    /// starts now, and it is kept to be sent again until it reaches the queue. `None` when the
    /// generation is no longer active or a probe is outstanding.
    pub(crate) fn issue_liveness_probe(
        &self,
        attempt: PendingConnectionAttempt,
        payload: MessagePayload,
        request: ProbeRequest,
        now_ms: i64,
    ) -> Result<Option<IssuedProbe>> {
        let identity = ProbeIdentity {
            tx_id: payload.transaction.tx_id,
            request,
        };
        let issued = self.with_active_slot(attempt, || {
            let mut liveness = self.peer_liveness()?;
            let Some(record) = liveness.generation_mut(attempt.peer, attempt.generation) else {
                return Ok(None);
            };
            if !record.clock.issue(identity, now_ms) {
                return Ok(None);
            }
            record.unsent = Some(payload.clone());
            Ok(Some(remaining(&record.clock, now_ms)))
        })?;
        Ok(issued.flatten().map(|deadline| IssuedProbe {
            attempt,
            identity,
            payload,
            deadline,
        }))
    }

    /// Record that `attempt`'s probe `identity` reached the peer's queue: it is not sent again.
    pub(crate) fn liveness_probe_queued(
        &self,
        attempt: PendingConnectionAttempt,
        identity: ProbeIdentity,
    ) -> Result<()> {
        self.with_active_slot(attempt, || {
            if let Some(record) = self
                .peer_liveness()?
                .generation_mut(attempt.peer, attempt.generation)
            {
                if record.clock.outstanding.map(|(outstanding, _)| outstanding) == Some(identity) {
                    record.unsent = None;
                }
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// Clear `provider`'s outstanding probe on its answer `(tx_id, request)`. Returns whether
    /// that probe was the outstanding one.
    pub(crate) fn answer_liveness_probe(
        &self,
        provider: Did,
        tx_id: uuid::Uuid,
        request: ProbeRequest,
    ) -> Result<bool> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(provider)? else {
                return Ok(false);
            };
            Ok(self
                .peer_liveness()?
                .generation_mut(provider, attempt.generation)
                .is_some_and(|liveness| liveness.answer(ProbeIdentity { tx_id, request })))
        })
    }

    pub(crate) fn peer_liveness_expiry(
        &self,
        attempt: PendingConnectionAttempt,
        now_ms: i64,
    ) -> Result<Option<PeerLivenessExpiry>> {
        self.with_active_slot(attempt, || {
            Ok(self
                .peer_liveness()?
                .generation(attempt.peer, attempt.generation)
                .and_then(|liveness| liveness.clock.expiry(now_ms)))
        })
        .map(Option::flatten)
    }

    /// Return how long an admitted peer has owned its current active generation.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn peer_connected_for_ms(&self, peer: Did, now_ms: i64) -> Result<Option<i64>> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(None);
            };
            Ok(self
                .peer_liveness()?
                .connected_for_ms(peer, attempt.generation, now_ms))
        })
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn mark_peer_liveness_connected_with_observer_for_test(
        &self,
        attempt: PendingConnectionAttempt,
        before_update: impl FnOnce(),
    ) -> Result<()> {
        self.observe_peer_liveness(attempt, PeerLivenessObservation::Connected, before_update)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_count_for_test(&self) -> Result<usize> {
        self.with_connection_lifecycle(|| Ok(self.peer_liveness()?.peers.len()))
    }

    /// Run `edit` on the liveness record of `peer`'s active generation.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    fn edit_peer_liveness_for_test<T>(
        &self,
        peer: Did,
        edit: impl FnOnce(&mut PeerLiveness) -> T,
    ) -> Result<T> {
        self.with_connection_lifecycle(|| {
            let attempt = self
                .active_attempt(peer)?
                .ok_or(Error::SwarmMissDidInTable(peer))?;
            let mut liveness = self.peer_liveness()?;
            let record = liveness
                .generation_mut(peer, attempt.generation)
                .ok_or(Error::SwarmMissDidInTable(peer))?;
            Ok(edit(record))
        })
    }

    /// When `peer`'s outstanding probe was issued, if one is.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_outstanding_since_for_test(
        &self,
        peer: Did,
    ) -> Result<Option<i64>> {
        self.edit_peer_liveness_for_test(peer, |liveness| liveness.clock.outstanding_since_ms())
    }

    /// Whether a probe of `peer` is due now.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_probe_due_for_test(&self, peer: Did) -> Result<bool> {
        let now_ms = get_epoch_ms_i64();
        self.edit_peer_liveness_for_test(peer, |liveness| liveness.clock.due(now_ms))
    }

    /// Whether `peer`'s outstanding probe reached its queue.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_probe_queued_for_test(&self, peer: Did) -> Result<bool> {
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.clock.outstanding_since_ms().is_some() && liveness.unsent.is_none()
        })
    }

    /// Make `peer`'s active generation carry an outstanding probe issued at `issued_ms`, which no
    /// answer matches, recording the generation if liveness has not yet.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_issued_at(
        &self,
        peer: Did,
        issued_ms: i64,
    ) -> Result<()> {
        let epoch = crate::message::ProvisionalEpoch::from_unix_seconds(0);
        let identity = ProbeIdentity {
            tx_id: uuid::Uuid::new_v4(),
            request: ProbeRequest::random_for_epoch(epoch),
        };
        self.with_connection_lifecycle(|| {
            let attempt = self
                .active_attempt(peer)?
                .ok_or(Error::SwarmMissDidInTable(peer))?;
            let mut liveness = PeerLiveness::new(attempt.generation, issued_ms);
            liveness.clock.issue(identity, issued_ms);
            self.peer_liveness()?.peers.insert(peer, liveness);
            Ok(())
        })
    }

    /// Make a probe of `peer` due now, as if its latest probe was issued a period ago.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_due(&self, peer: Did) -> Result<()> {
        let issued_ms = get_epoch_ms_i64() - PEER_LIVENESS_PERIOD_MS;
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.clock = ProbeClock::admitted_at(issued_ms);
        })
    }

    /// Restart `peer`'s probe period now, as if a probe had just been answered: no probe of it
    /// is due for a period.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn restart_peer_liveness_period_for_test(&self, peer: Did) -> Result<()> {
        let now_ms = get_epoch_ms_i64();
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.clock = ProbeClock::admitted_at(now_ms);
        })
    }

    /// Age `peer`'s latest authenticated inbound payload, for retention fixtures.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_last_inbound_at(&self, peer: Did, last_inbound_ms: i64) -> Result<()> {
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.last_inbound_ms = last_inbound_ms;
        })
    }

    /// Age the current generation for native retention and liveness fixtures.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_connected_at(&self, peer: Did, connected_at_ms: i64) -> Result<()> {
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.connected_at_ms = connected_at_ms;
        })
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test_composed_model;

#[cfg(test)]
mod tests {
    use super::ProbeClock;
    use super::ProbeIdentity;
    use super::PEER_LIVENESS_PERIOD_MS;
    use super::PEER_LIVENESS_TIMEOUT_MS;
    use crate::message::ProbeRequest;
    use crate::message::ProvisionalEpoch;

    /// A fresh probe identity, as production issues one.
    fn probe() -> ProbeIdentity {
        ProbeIdentity {
            tx_id: uuid::Uuid::new_v4(),
            request: ProbeRequest::random_for_epoch(ProvisionalEpoch::from_unix_seconds(0)),
        }
    }

    /// One outstanding: a second issue is refused, so the first answer stays acceptable.
    #[test]
    fn test_a_probe_is_never_issued_over_an_outstanding_one() {
        let (first, second) = (probe(), probe());
        let mut clock = ProbeClock::admitted_at(0);
        assert!(!clock.due(PEER_LIVENESS_PERIOD_MS - 1));
        assert!(clock.due(PEER_LIVENESS_PERIOD_MS));
        assert!(clock.issue(first, PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.due(3 * PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.issue(second, 3 * PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.answer(second));
        assert!(clock.answer(first));
        assert_eq!(clock.outstanding_since_ms(), None);
    }

    /// Deadline from issue: the outstanding probe expires `TIMEOUT` after its issue, whatever
    /// happens meanwhile, and only its answer clears it.
    #[test]
    fn test_the_deadline_counts_from_issue_and_only_the_answer_clears_it() {
        let outstanding = probe();
        let mut clock = ProbeClock::admitted_at(0);
        assert!(clock.issue(outstanding, 10));
        assert!(clock.expiry(10 + PEER_LIVENESS_TIMEOUT_MS - 1).is_none());
        assert!(clock.expiry(10 + PEER_LIVENESS_TIMEOUT_MS).is_some());
        assert!(!clock.answer(probe()));
        assert!(clock.answer(outstanding));
        assert!(clock.expiry(10 + PEER_LIVENESS_TIMEOUT_MS).is_none());
    }

    /// Unsent is resent, never withdrawn: until its answer, an issued probe stays outstanding
    /// with its deadline from issue, however long it takes to reach the queue (#913 R10 M7).
    #[test]
    fn test_an_unanswered_probe_keeps_its_deadline_from_issue() {
        let mut clock = ProbeClock::admitted_at(0);
        assert!(clock.issue(probe(), PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.due(3 * PEER_LIVENESS_PERIOD_MS));
        assert_eq!(
            clock.remaining_ms(PEER_LIVENESS_PERIOD_MS),
            Some(PEER_LIVENESS_TIMEOUT_MS)
        );
        assert!(clock
            .expiry(PEER_LIVENESS_PERIOD_MS + PEER_LIVENESS_TIMEOUT_MS)
            .is_some());
    }
}
