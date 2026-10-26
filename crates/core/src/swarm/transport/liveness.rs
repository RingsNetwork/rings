//! Overlay liveness of admitted peers, judged by probes alone.
//!
//! Each admitted generation carries a [`ProbeClock`], a pure state machine over the probe's
//! challenge `c`:
//!
//! ```text
//!   Idle(last) ──issue(c, t)  [t − last ≥ PERIOD]──▶ Outstanding(c, t, unsent)
//!   Outstanding(c, t, unsent) ──queued(c)─────────▶ Outstanding(c, t, queued)
//!   Outstanding(c, t, _) ──answer(c)──────────────▶ Idle(t)
//!   Outstanding(c, t, _) ──now − t ≥ TIMEOUT──────▶ expired: the peer is evicted
//! ```
//!
//! Laws:
//! - **One outstanding.** `issue` is refused while a probe is outstanding, so a later probe
//!   never makes an earlier answer unacceptable.
//! - **Deadline from issue.** The deadline counts from the instant the probe is issued; nothing
//!   extends it, and nothing waits for the probe to leave the queue.
//! - **Answer only.** Only the answer carrying the outstanding challenge clears it; a peer's
//!   other inbound traffic proves only that it sends, not that it takes this end's control
//!   traffic. Inbound traffic is recorded for retention alone.
//! - **Credit-blind.** No flow-control state is read. A peer that withholds its control lane's
//!   credit keeps the probe in its own queue, so the probe goes unanswered and the peer is
//!   evicted by the ordinary rule; a peer slow on a data lane answers on lane 0 and is kept.
//! - **Probe slot.** A transfer of an outstanding probe is an [`IssuedProbe`], which only this
//!   module builds; it takes no shared outbound capacity (the law of `outbound::sender_bounded`),
//!   and it waits for credit until the answer deadline itself.
//! - **Unsent is resent, never withdrawn.** A probe that did not reach the queue (no ready
//!   connection, or a step cut short) stays outstanding with its deadline, and is sent again on
//!   the next pass: the same challenge, signed anew under a fresh transaction, so its sequence
//!   is never one its peer's replay window has passed. A peer that keeps its connection unready
//!   at each pass gains nothing: its deadline runs from the first issue.
//! - **Single flight.** Probes are issued and sent only within a [`ProbePass`], of which at
//!   most one runs at a time, so two passes never send one unsent probe twice. A pass cut short
//!   releases its right with the probe still unsent, for the next pass.
//! - **Pinned.** A probe rides only the connection of the generation it judges
//!   (`SenderBound::Probe`): after a reconnect, the successor's clock would not know its answer.
//! - **Chain neighbours (#1070).** This node's relay chain neighbours, `p₁` and `n₁` (#777), are
//!   probed every [`CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS`] instead of every
//!   [`PEER_LIVENESS_PERIOD_MS`], on the same clock, so a peer is never probed twice. A chain
//!   neighbour whose probe is [`CHAIN_NEIGHBOUR_SUSPICION_MS`] unanswered is *suspected* for
//!   relay custody alone, and the chain moves past it; it is evicted only by the ordinary
//!   [`PEER_LIVENESS_TIMEOUT_MS`], and two on-time answers in a row clear the suspicion
//!   (`Law (Clearing)` of `dht::relay_holder`). A silent loss of a chain neighbour is so
//!   detected within `T_d ≤ P_c + τ_c = 10 s`, and a slow but live one costs a chain shift and
//!   back, never a copy, at most twice per `τ_c + 2·P_c = 12.5 s`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::MutexGuard;
use std::time::Duration;

use futures::lock::MutexGuard as FuturesMutexGuard;

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
/// Interval between two probes of a relay chain neighbour (#1070), `P_c`: with
/// [`CHAIN_NEIGHBOUR_SUSPICION_MS`] it bounds a chain neighbour's silent-loss detection at
/// `P_c + τ_c = 10 s`, the `D = 10 s` column of the replication table of #777.
pub(crate) const CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS: i64 = 2_500;
/// How long a relay chain neighbour's probe goes unanswered, counted from its issue, before the
/// neighbour is suspected for relay custody (#1070), `τ_c`. Suspicion moves the chain past it
/// and evicts nothing.
pub(crate) const CHAIN_NEIGHBOUR_SUSPICION_MS: i64 = 7_500;

/// Which peers a probe pass probes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProbeScope {
    /// Every admitted peer: the stabilization pass.
    Every,
    /// This node's relay chain neighbours alone: the chain liveness pass, run every
    /// [`CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS`].
    ChainNeighbours,
}

/// The outstanding probe of a generation: its challenge, its issue instant, and whether it
/// reached its peer's queue. The challenge is the probe's identity: a probe sent again is
/// signed anew, under a fresh transaction and sequence, and its answer still carries it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Outstanding {
    /// The challenge the answer must carry.
    pub(crate) request: ProbeRequest,
    /// When the probe was issued: its deadline counts from here.
    pub(crate) issued_ms: i64,
    /// Whether a transfer of it reached the peer's queue; until then it is sent again.
    pub(crate) queued: bool,
}

/// The probe clock of one admitted generation; see the module laws.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ProbeClock {
    /// When the latest probe was issued, or the generation was admitted before any probe.
    last_issued_ms: i64,
    /// The outstanding probe, if one is.
    outstanding: Option<Outstanding>,
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

    /// Whether a probe is due at `now_ms` under `period_ms`: none is outstanding and a period
    /// has passed since the latest issue.
    pub(crate) fn due(&self, now_ms: i64, period_ms: i64) -> bool {
        self.outstanding.is_none() && now_ms.saturating_sub(self.last_issued_ms) >= period_ms
    }

    /// Issue the probe of challenge `request` at `now_ms`. Refused (`false`) while another
    /// probe is outstanding.
    pub(crate) fn issue(&mut self, request: ProbeRequest, now_ms: i64) -> bool {
        if self.outstanding.is_some() {
            return false;
        }
        self.outstanding = Some(Outstanding {
            request,
            issued_ms: now_ms,
            queued: false,
        });
        self.last_issued_ms = now_ms;
        true
    }

    /// The challenge of the outstanding probe while no transfer of it reached the queue: the
    /// probe to send again.
    pub(crate) fn unsent(&self) -> Option<ProbeRequest> {
        self.outstanding
            .filter(|outstanding| !outstanding.queued)
            .map(|outstanding| outstanding.request)
    }

    /// Record that a transfer of the probe of challenge `request` reached the peer's queue.
    pub(crate) fn queued(&mut self, request: ProbeRequest) {
        if let Some(outstanding) = self.outstanding.as_mut() {
            outstanding.queued |= outstanding.request == request;
        }
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

    /// Clear the outstanding probe on its answer carrying `request`. Returns whether that probe
    /// was the outstanding one.
    pub(crate) fn answer(&mut self, request: ProbeRequest) -> bool {
        if self.outstanding.map(|outstanding| outstanding.request) != Some(request) {
            return false;
        }
        self.outstanding = None;
        true
    }

    /// When the outstanding probe was issued, if one is.
    pub(crate) fn outstanding_since_ms(&self) -> Option<i64> {
        self.outstanding.map(|outstanding| outstanding.issued_ms)
    }

    /// Whether the outstanding probe is [`CHAIN_NEIGHBOUR_SUSPICION_MS`] old at `now_ms`: a
    /// chain neighbour that has not answered it is suspected for relay custody.
    pub(crate) fn suspected(&self, now_ms: i64) -> bool {
        self.outstanding_since_ms().is_some_and(|issued_ms| {
            now_ms.saturating_sub(issued_ms) >= CHAIN_NEIGHBOUR_SUSPICION_MS
        })
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

/// One transfer of a probe its generation's clock issued and holds outstanding: built only
/// here, by [`SwarmTransport::issue_liveness_probe`] and [`SwarmTransport::resend_liveness_probe`],
/// which is what lets its transfer bypass the shared outbound capacity. Its fields are private,
/// so no other code can forge one. It is an owned value: an answer or an eviction while it is
/// in flight leaves it to be sent and ignored.
pub(crate) struct IssuedProbe {
    attempt: PendingConnectionAttempt,
    request: ProbeRequest,
    payload: MessagePayload,
    /// How long the transfer may wait for credit: what is left of the answer deadline.
    deadline: Duration,
}

impl IssuedProbe {
    /// The generation the probe judges.
    pub(crate) fn attempt(&self) -> PendingConnectionAttempt {
        self.attempt
    }

    /// The probe's challenge, its identity.
    pub(crate) fn request(&self) -> ProbeRequest {
        self.request
    }

    /// The generation, payload and transfer deadline the egress sends.
    pub(super) fn into_send(self) -> (PendingConnectionAttempt, MessagePayload, Duration) {
        (self.attempt, self.payload, self.deadline)
    }
}

/// One step of a probe pass.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ProbeWork {
    /// Issue a fresh probe of this generation.
    Issue(PendingConnectionAttempt),
    /// Sign again, under a fresh transaction, and send the outstanding probe of challenge
    /// `request` that has not reached the queue: a payload kept from its issue would carry a
    /// sequence its peer's replay window may have passed meanwhile.
    Resend {
        attempt: PendingConnectionAttempt,
        request: ProbeRequest,
    },
}

/// One probe pass: the exclusive right to issue and send this node's probes, held for the
/// pass, and the work it found. The right is released when the pass is dropped, completed or
/// cut short.
pub(crate) struct ProbePass<'transport> {
    _exclusive: FuturesMutexGuard<'transport, ()>,
    work: Vec<ProbeWork>,
}

impl ProbePass<'_> {
    /// What the pass must do; see [`ProbeWork`].
    pub(crate) fn work(&self) -> &[ProbeWork] {
        &self.work
    }
}

/// What is left of `clock`'s outstanding probe deadline at `now_ms`, as a transfer deadline.
fn remaining(clock: &ProbeClock, now_ms: i64) -> Duration {
    let remaining_ms = clock.remaining_ms(now_ms).unwrap_or(0);
    Duration::from_millis(u64::try_from(remaining_ms).unwrap_or(0))
}

/// One admitted generation's liveness record.
#[derive(Clone, Debug)]
struct PeerLiveness {
    generation: u64,
    connected_at_ms: i64,
    /// The latest authenticated inbound payload, read by retention, never by liveness.
    last_inbound_ms: i64,
    clock: ProbeClock,
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
        }
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

    /// What the probe pass of `scope` must do at `now_ms`: issue a probe of every active
    /// generation in scope that is due one, a chain neighbour of `chain` under the chain period
    /// and every other peer under the ordinary one, and send again every outstanding probe in
    /// scope that has not reached the queue.
    fn probe_work(
        &mut self,
        active: &ActiveConnectionSet,
        now_ms: i64,
        chain: &BTreeSet<Did>,
        scope: ProbeScope,
    ) -> Vec<ProbeWork> {
        self.retain_active(active);
        for attempt in active.iter() {
            self.peers
                .entry(attempt.peer)
                .or_insert_with(|| PeerLiveness::new(attempt.generation, now_ms));
        }
        active
            .iter()
            .filter(|attempt| scope == ProbeScope::Every || chain.contains(&attempt.peer))
            .filter_map(|attempt| {
                let liveness = self.peers.get(&attempt.peer)?;
                let period_ms = if chain.contains(&attempt.peer) {
                    CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS
                } else {
                    PEER_LIVENESS_PERIOD_MS
                };
                if liveness.clock.due(now_ms, period_ms) {
                    return Some(ProbeWork::Issue(attempt));
                }
                let request = liveness.clock.unsent()?;
                Some(ProbeWork::Resend { attempt, request })
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

    /// This node's relay chain neighbours, the peers probed on the chain period (#1070): none
    /// while it holds no relay mail, since it is then no member of any chain.
    fn chain_neighbour_set(&self) -> Result<BTreeSet<Did>> {
        if !self.dht.holds_relay_mail() {
            return Ok(BTreeSet::new());
        }
        let (previous, next) = self.dht.relay_chain_neighbours()?;
        Ok(previous.into_iter().chain(next).collect())
    }

    /// Open the probe pass of `scope` at `now_ms`, with what it must do. `None` while another
    /// pass runs: that pass does the work.
    pub(crate) fn liveness_probe_pass(
        &self,
        now_ms: i64,
        scope: ProbeScope,
    ) -> Result<Option<ProbePass<'_>>> {
        let Some(exclusive) = self.probe_pass.try_lock() else {
            return Ok(None);
        };
        let chain = self.chain_neighbour_set()?;
        let work = self.with_connection_lifecycle(|| {
            let active = self.active_connections()?;
            Ok(self
                .peer_liveness()?
                .probe_work(&active, now_ms, &chain, scope))
        })?;
        Ok(Some(ProbePass {
            _exclusive: exclusive,
            work,
        }))
    }

    /// Issue `payload`, carrying `request`, as `attempt`'s probe at `now_ms`: its deadline
    /// starts now, and it is sent again until a transfer reaches the queue. `None` when the
    /// generation is no longer active or a probe is outstanding.
    pub(crate) fn issue_liveness_probe(
        &self,
        _pass: &ProbePass<'_>,
        attempt: PendingConnectionAttempt,
        payload: MessagePayload,
        request: ProbeRequest,
        now_ms: i64,
    ) -> Result<Option<IssuedProbe>> {
        self.liveness_probe_transfer(attempt, payload, request, now_ms, |clock| {
            clock.issue(request, now_ms)
        })
    }

    /// Send `payload`, the outstanding probe of challenge `request` signed anew, as `attempt`'s
    /// probe at `now_ms`, under the deadline of its first issue. `None` when the generation is
    /// no longer active, or the probe was answered or reached the queue meanwhile.
    pub(crate) fn resend_liveness_probe(
        &self,
        _pass: &ProbePass<'_>,
        attempt: PendingConnectionAttempt,
        payload: MessagePayload,
        request: ProbeRequest,
        now_ms: i64,
    ) -> Result<Option<IssuedProbe>> {
        self.liveness_probe_transfer(attempt, payload, request, now_ms, |clock| {
            clock.unsent() == Some(request)
        })
    }

    /// One transfer of `attempt`'s probe of challenge `request`, if `admits` admits it on the
    /// generation's clock, under what is left of the probe's deadline at `now_ms`.
    fn liveness_probe_transfer(
        &self,
        attempt: PendingConnectionAttempt,
        payload: MessagePayload,
        request: ProbeRequest,
        now_ms: i64,
        admits: impl FnOnce(&mut ProbeClock) -> bool,
    ) -> Result<Option<IssuedProbe>> {
        let deadline = self.with_active_slot(attempt, || {
            let mut liveness = self.peer_liveness()?;
            let Some(record) = liveness.generation_mut(attempt.peer, attempt.generation) else {
                return Ok(None);
            };
            Ok(admits(&mut record.clock).then(|| remaining(&record.clock, now_ms)))
        })?;
        Ok(deadline.flatten().map(|deadline| IssuedProbe {
            attempt,
            request,
            payload,
            deadline,
        }))
    }

    /// Record that a transfer of `attempt`'s probe of challenge `request` reached the peer's
    /// queue: it is not sent again.
    pub(crate) fn liveness_probe_queued(
        &self,
        attempt: PendingConnectionAttempt,
        request: ProbeRequest,
    ) -> Result<()> {
        self.with_active_slot(attempt, || {
            if let Some(record) = self
                .peer_liveness()?
                .generation_mut(attempt.peer, attempt.generation)
            {
                record.clock.queued(request);
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// Clear `provider`'s outstanding probe on its answer carrying `request`, and count the
    /// answer toward clearing a suspicion of `provider` for relay custody, on time iff it came
    /// within the chain timeout (#1070, `Law (Clearing)`). Returns whether that probe was the
    /// outstanding one.
    pub(crate) fn answer_liveness_probe(
        &self,
        provider: Did,
        request: ProbeRequest,
    ) -> Result<bool> {
        let now_ms = get_epoch_ms_i64();
        let answered = self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(provider)? else {
                return Ok(None);
            };
            let mut liveness = self.peer_liveness()?;
            let Some(record) = liveness.generation_mut(provider, attempt.generation) else {
                return Ok(None);
            };
            let on_time = !record.clock.suspected(now_ms);
            Ok(record.clock.answer(request).then_some(on_time))
        })?;
        if let Some(on_time) = answered {
            self.dht.observe_chain_neighbour_answer(provider, on_time)?;
        }
        Ok(answered.is_some())
    }

    /// This node's relay chain neighbours whose outstanding probe is
    /// [`CHAIN_NEIGHBOUR_SUSPICION_MS`] unanswered at `now_ms`: to suspect for relay custody.
    pub(crate) fn unanswering_chain_neighbours(&self, now_ms: i64) -> Result<Vec<Did>> {
        let chain = self.chain_neighbour_set()?;
        self.with_connection_lifecycle(|| {
            let liveness = self.peer_liveness()?;
            let mut unanswering = Vec::new();
            for peer in chain {
                let Some(attempt) = self.active_attempt(peer)? else {
                    continue;
                };
                if liveness
                    .generation(peer, attempt.generation)
                    .is_some_and(|record| record.clock.suspected(now_ms))
                {
                    unanswering.push(peer);
                }
            }
            Ok(unanswering)
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
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.clock.due(now_ms, PEER_LIVENESS_PERIOD_MS)
        })
    }

    /// Whether `peer`'s outstanding probe reached its queue.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn peer_liveness_probe_queued_for_test(&self, peer: Did) -> Result<bool> {
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness
                .clock
                .outstanding
                .is_some_and(|outstanding| outstanding.queued)
        })
    }

    /// Make `peer`'s active generation carry an outstanding probe issued at `issued_ms`,
    /// recording the generation if liveness has not yet; its challenge is returned, which only
    /// a test can answer.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_issued_at(
        &self,
        peer: Did,
        issued_ms: i64,
    ) -> Result<ProbeRequest> {
        let epoch = crate::message::ProvisionalEpoch::from_unix_seconds(0);
        let request = ProbeRequest::random_for_epoch(epoch);
        self.with_connection_lifecycle(|| {
            let attempt = self
                .active_attempt(peer)?
                .ok_or(Error::SwarmMissDidInTable(peer))?;
            let mut liveness = PeerLiveness::new(attempt.generation, issued_ms);
            liveness.clock.issue(request, issued_ms);
            self.peer_liveness()?.peers.insert(peer, liveness);
            Ok(request)
        })
    }

    /// Restart `peer`'s probe clock with nothing outstanding, as if its latest probe was answered
    /// `ago_ms` before now: a probe is due iff `ago_ms` is a period or more.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn restart_peer_liveness_period_for_test(
        &self,
        peer: Did,
        ago_ms: i64,
    ) -> Result<()> {
        let issued_ms = get_epoch_ms_i64() - ago_ms;
        self.edit_peer_liveness_for_test(peer, |liveness| {
            liveness.clock = ProbeClock::admitted_at(issued_ms);
        })
    }

    /// Make a probe of `peer` due now: [`Self::restart_peer_liveness_period_for_test`] a period
    /// ago.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_liveness_probe_due(&self, peer: Did) -> Result<()> {
        self.restart_peer_liveness_period_for_test(peer, PEER_LIVENESS_PERIOD_MS)
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
    use super::CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS;
    use super::CHAIN_NEIGHBOUR_SUSPICION_MS;
    use super::PEER_LIVENESS_PERIOD_MS;
    use super::PEER_LIVENESS_TIMEOUT_MS;
    use crate::message::ProbeRequest;
    use crate::message::ProvisionalEpoch;

    /// A fresh probe challenge, as production issues one.
    fn probe() -> ProbeRequest {
        ProbeRequest::random_for_epoch(ProvisionalEpoch::from_unix_seconds(0))
    }

    /// One outstanding: a second issue is refused, so the first answer stays acceptable.
    /// A chain neighbour is probed on the chain period, on the same clock as any peer, and is
    /// suspected once its probe is the chain timeout unanswered, well before the eviction
    /// deadline (#1070).
    #[test]
    fn test_a_chain_neighbour_is_probed_sooner_and_suspected_before_eviction() {
        let request = probe();
        let mut clock = ProbeClock::admitted_at(0);
        assert!(!clock.due(
            CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS - 1,
            CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS
        ));
        assert!(clock.due(
            CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS,
            CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS
        ));
        assert!(!clock.due(CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS, PEER_LIVENESS_PERIOD_MS));
        assert!(clock.issue(request, 0));
        assert!(!clock.suspected(CHAIN_NEIGHBOUR_SUSPICION_MS - 1));
        assert!(clock.suspected(CHAIN_NEIGHBOUR_SUSPICION_MS));
        assert!(
            clock.expiry(CHAIN_NEIGHBOUR_SUSPICION_MS).is_none(),
            "suspicion evicts nothing"
        );
        assert!(clock.expiry(PEER_LIVENESS_TIMEOUT_MS).is_some());
        assert!(clock.answer(request));
        assert!(
            !clock.suspected(PEER_LIVENESS_TIMEOUT_MS),
            "an answer clears the suspicion"
        );
    }

    /// The chain bounds: detection within ten seconds, the probe period below the suspicion
    /// timeout, and suspicion before eviction.
    #[test]
    fn test_the_chain_bounds_detect_a_silent_loss_within_ten_seconds() {
        assert_eq!(
            CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS + CHAIN_NEIGHBOUR_SUSPICION_MS,
            10_000
        );
        assert!(CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS < CHAIN_NEIGHBOUR_SUSPICION_MS);
        assert!(CHAIN_NEIGHBOUR_SUSPICION_MS < PEER_LIVENESS_TIMEOUT_MS);
    }

    #[test]
    fn test_a_probe_is_never_issued_over_an_outstanding_one() {
        let (first, second) = (probe(), probe());
        let mut clock = ProbeClock::admitted_at(0);
        assert!(!clock.due(PEER_LIVENESS_PERIOD_MS - 1, PEER_LIVENESS_PERIOD_MS));
        assert!(clock.due(PEER_LIVENESS_PERIOD_MS, PEER_LIVENESS_PERIOD_MS));
        assert!(clock.issue(first, PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.due(3 * PEER_LIVENESS_PERIOD_MS, PEER_LIVENESS_PERIOD_MS));
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
    /// with its deadline from issue, however long it takes to reach the queue.
    #[test]
    fn test_an_unanswered_probe_keeps_its_deadline_from_issue() {
        let mut clock = ProbeClock::admitted_at(0);
        assert!(clock.issue(probe(), PEER_LIVENESS_PERIOD_MS));
        assert!(!clock.due(3 * PEER_LIVENESS_PERIOD_MS, PEER_LIVENESS_PERIOD_MS));
        assert_eq!(
            clock.remaining_ms(PEER_LIVENESS_PERIOD_MS),
            Some(PEER_LIVENESS_TIMEOUT_MS)
        );
        assert!(clock
            .expiry(PEER_LIVENESS_PERIOD_MS + PEER_LIVENESS_TIMEOUT_MS)
            .is_some());
    }
}
