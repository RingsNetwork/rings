//! Run-owned bootstrap reachability supervision for `rings run`.
//!
//! `rings connect node` and `rings connect seed` establish a connection once. This module gives
//! the native daemon a set of *managed* bootstrap targets that it keeps reachable for the life
//! of the process: whenever a target stops being reachable through the overlay, the supervisor
//! redials it through its HTTP endpoint, first in a bounded rapid burst and then on a slow
//! jittered cadence, and resets once the target is reachable again. Ordinary neighbour and
//! finger edges stay under Chord stabilization; only the configured targets are supervised, and
//! a target reachable through other peers is never forced into a direct edge.
//!
//! The design splits into a pure core and an effect shell:
//!
//! - `schedule::BootstrapSchedule` (private) — the retry state machine, a function of explicit
//!   time; its module docs carry the phase diagram and laws.
//! - `BootstrapPort` (crate-private) — the two effects a turn needs: assess reachability and
//!   dial; `ProcessorPort` is the production port over a live processor.
//! - [`BootstrapSupervisor`] — the shell: wakes on deadlines, target losses and finished turns;
//!   runs at most one turn per target; exits on the run's stop token.
//! - `evidence::ReachabilityEvidence` (crate-private) — what the swarm reports about the
//!   targets: lookup reports for the probe, admissions the dial waits for, and retirements the
//!   supervisor reads as losses; written by the [`Backend`] through [`BackendObserver`], which
//!   the supervisor hands out as [`BootstrapSupervisor::observer`].
//!
//! ```text
//!                    ┌────────────────────────── run loop ──────────────────────────┐
//!  StopToken ──stop─▶│ drain losses ─▶ notice_loss ─▶ ∀ due target: begin ─▶ push turn│
//!  Losses ─────wake─▶│ wait { stop | wake | turn finished | next deadline } ─▶ settle │
//!                    └──────────────────────────────────────────────────────────────┘
//!
//!  turn(t) := reachable(t) ? Reachable
//!           : dial(t) = Ok ? Reachable : (handshake already in flight ? Deferred : DialFailed)
//! ```
//!
//! Lost-wakeup freedom: every wait is preceded by a drain of the loss record, and a loss
//! recorded between the drain and the wait leaves a stored permit, so no loss is missed. A turn
//! is bounded: the port bounds its probe, the endpoint's resolution, each of its two HTTP
//! requests and the admission wait with a timeout of its own, and the core bounds the offer
//! (ICE gathering) and the routed send it performs on the port's behalf.
//!
//! Shutdown: the loop returns on the first stop observation and its in-flight turns are
//! dropped with it, cancelling any probe or handshake in progress. A pending connection attempt
//! the core already reserved expires under the core's own pending timeout. Transient failures
//! stay inside the loop; only configuration is validated up front and fails fast.
//!
//! [`Backend`]: crate::extension::Backend
//! [`BackendObserver`]: crate::extension::BackendObserver

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rings_core::dht::Did;
use tokio::time::Instant;

use self::evidence::PeerLosses;
use self::schedule::BootstrapSchedule;
use self::schedule::TurnOutcome;
use crate::error::Error;
use crate::error::Result;
use crate::extension::BackendObserver;
use crate::native::config::BootstrapConfig;
use crate::prelude::StopToken;
use crate::processor::Processor;
use crate::remote_endpoint::RemoteRpcEndpoint;
use crate::seed::SeedPeerError;
use crate::seed::ValidatedSeedPeer;

mod evidence;
mod probe;
mod schedule;
#[cfg(test)]
mod tests;

pub(crate) use self::evidence::ReachabilityEvidence;
pub(crate) use self::probe::ProcessorPort;

/// Why a seed entry cannot be a managed target.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum BootstrapTargetError {
    /// The entry cannot be dialed at all.
    #[error(transparent)]
    Peer(#[from] SeedPeerError),
    /// The entry names the node itself.
    #[error("bootstrap peer {0} is this node itself")]
    LocalNode(Did),
    /// The DID is listed again with a different endpoint or token.
    #[error("bootstrap peer {0} is listed with conflicting entries")]
    ConflictingEntries(Did),
    /// The endpoint is listed again under a different DID.
    #[error("bootstrap endpoint {0} is listed under two DIDs")]
    EndpointUnderTwoDids(RemoteRpcEndpoint),
}

/// How two managed targets overlap, in decreasing order of agreement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Overlap {
    /// Same DID, endpoint and token: one target listed twice.
    Verbatim,
    /// Same DID behind another endpoint or token.
    SameDid,
    /// Same endpoint claimed for another DID.
    SameEndpoint,
}

/// How `target` overlaps with `known`, if at all.
fn overlap(known: &ValidatedSeedPeer, target: &ValidatedSeedPeer) -> Option<Overlap> {
    if known == target {
        Some(Overlap::Verbatim)
    } else if known.did() == target.did() {
        Some(Overlap::SameDid)
    } else if known.endpoint() == target.endpoint() {
        Some(Overlap::SameEndpoint)
    } else {
        None
    }
}

/// Validated managed targets: one per DID, one per endpoint, none of them the local node.
#[derive(Debug)]
pub struct BootstrapTargets(Vec<ValidatedSeedPeer>);

impl BootstrapTargets {
    /// Validate `config` for the node `local`: every entry validates as a seed peer and none
    /// is `local` itself. An entry repeated verbatim (as when the same seed document feeds both
    /// the config and `--bootstrap-seed`) is merged; a DID listed with a different endpoint or
    /// token, or an endpoint listed under two DIDs, is rejected as ambiguous, since one
    /// endpoint answers as exactly one DID. Endpoint identity is the full URL: two nodes may
    /// share an origin behind path routing, and an entry whose endpoint answers as another node
    /// is refused at dial time by the DID pin.
    pub fn from_config(config: BootstrapConfig, local: Did) -> Result<Self> {
        let mut targets: Vec<ValidatedSeedPeer> = Vec::with_capacity(config.peers.len());
        for peer in config.peers {
            let target = ValidatedSeedPeer::try_from(peer).map_err(BootstrapTargetError::Peer)?;
            if target.did() == local {
                return Err(BootstrapTargetError::LocalNode(target.did()).into());
            }
            match targets.iter().find_map(|known| overlap(known, &target)) {
                Some(Overlap::Verbatim) => continue,
                Some(Overlap::SameDid) => {
                    return Err(BootstrapTargetError::ConflictingEntries(target.did()).into());
                }
                Some(Overlap::SameEndpoint) => {
                    return Err(BootstrapTargetError::EndpointUnderTwoDids(
                        target.endpoint().clone(),
                    )
                    .into());
                }
                None => targets.push(target),
            }
        }
        Ok(Self(targets))
    }

    /// Whether there is nothing to supervise.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of managed targets.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// Why a dial did not connect the target.
#[derive(Debug)]
pub(crate) enum DialFailure {
    /// The core already holds a connection attempt to the target; nothing was attempted.
    InFlight,
    /// The handshake or its admission failed.
    Failed(Error),
}

/// Effects one supervisor turn performs on a target.
#[async_trait]
pub(crate) trait BootstrapPort: Send + Sync {
    /// Whether `target` is reachable through the overlay right now.
    async fn reachable(&self, target: &ValidatedSeedPeer) -> bool;

    /// Redial `target` through its HTTP endpoint; `Ok` once the peer is admitted.
    async fn dial(&self, target: &ValidatedSeedPeer) -> std::result::Result<(), DialFailure>;
}

/// The run-owned supervisor; see the module diagram.
pub struct BootstrapSupervisor {
    targets: BTreeMap<Did, Arc<ValidatedSeedPeer>>,
    port: Arc<dyn BootstrapPort>,
    evidence: Arc<ReachabilityEvidence>,
    schedule: BootstrapSchedule,
    origin: Instant,
}

/// In-flight turns, each resolving to the target it served and its outcome.
type Turns = FuturesUnordered<BoxFuture<'static, (Did, TurnOutcome)>>;

/// What woke the run loop.
enum Wake {
    /// The run's stop token was observed.
    Stop,
    /// A target loss was recorded.
    Loss,
    /// A turn finished for the target.
    Turn(Did, TurnOutcome),
    /// The next scheduled deadline passed.
    Deadline,
}

impl BootstrapSupervisor {
    /// Supervision of `targets` over a live processor, or `None` when there is nothing to
    /// supervise, in which case no observer is needed either.
    pub fn over_processor(targets: BootstrapTargets, processor: Arc<Processor>) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        let evidence = Arc::new(ReachabilityEvidence::default());
        let port = Arc::new(ProcessorPort::new(processor, evidence.clone()));
        Some(Self::new(targets, port, evidence, rand::random()))
    }

    /// Supervisor over `port` reading `evidence`, with every target due immediately and jitter
    /// seeded by `jitter_seed`.
    pub(crate) fn new(
        targets: BootstrapTargets,
        port: Arc<dyn BootstrapPort>,
        evidence: Arc<ReachabilityEvidence>,
        jitter_seed: u64,
    ) -> Self {
        let targets: BTreeMap<Did, Arc<ValidatedSeedPeer>> = targets
            .0
            .into_iter()
            .map(|target| (target.did(), Arc::new(target)))
            .collect();
        let schedule = BootstrapSchedule::new(targets.keys().copied(), jitter_seed);
        Self {
            targets,
            port,
            evidence,
            schedule,
            origin: Instant::now(),
        }
    }

    /// The observer the daemon installs on its backend so the swarm's reports reach this
    /// supervisor.
    pub fn observer(&self) -> Arc<dyn BackendObserver> {
        self.evidence.clone()
    }

    /// Run until `stop` is observed; never returns otherwise.
    pub async fn run(mut self, stop: StopToken) {
        let mut turns = Turns::new();
        loop {
            self.drain_losses();
            self.start_due_turns(&mut turns);
            let deadline = self.schedule.next_deadline_ms().and_then(|deadline_ms| {
                self.origin.checked_add(Duration::from_millis(deadline_ms))
            });
            match wait_for_wake(&stop, self.evidence.losses(), &mut turns, deadline).await {
                Wake::Stop => return,
                Wake::Loss | Wake::Deadline => {}
                Wake::Turn(target, outcome) => {
                    self.schedule.settle(target, outcome, self.now_ms());
                }
            }
        }
    }

    /// Milliseconds since the supervisor started, saturating.
    fn now_ms(&self) -> u64 {
        schedule::duration_ms(self.origin.elapsed())
    }

    /// Fold every recorded target loss into the schedule.
    fn drain_losses(&mut self) {
        let lost = match self.evidence.losses().take() {
            Ok(lost) => lost,
            Err(error) => {
                tracing::error!(%error, "bootstrap loss record unavailable");
                return;
            }
        };
        let now_ms = self.now_ms();
        for target in lost {
            if self.schedule.notice_loss(target, now_ms) {
                tracing::info!(%target, "bootstrap target left the local DHT; reassessing");
            }
        }
    }

    /// Push one turn for every target that is due.
    fn start_due_turns(&mut self, turns: &mut Turns) {
        let now_ms = self.now_ms();
        for (did, target) in &self.targets {
            if self.schedule.begin_if_due(*did, now_ms) {
                turns.push(Box::pin(turn(self.port.clone(), target.clone())));
            }
        }
    }
}

/// Block until the run should act again.
///
/// An empty turn set yields `None` at once, which disables its branch rather than completing
/// it, so an idle supervisor sleeps until its deadline, a loss, or the stop token. Every future
/// here is cancel-safe: the stop and loss waits register before re-checking, the turn stream
/// hands back completed turns one at a time, and the sleep holds no state.
async fn wait_for_wake(
    stop: &StopToken,
    losses: &PeerLosses,
    turns: &mut Turns,
    deadline: Option<Instant>,
) -> Wake {
    tokio::select! {
        _ = stop.stopped() => Wake::Stop,
        _ = losses.woken() => Wake::Loss,
        Some((target, outcome)) = turns.next() => Wake::Turn(target, outcome),
        _ = sleep_until_or_forever(deadline) => Wake::Deadline,
    }
}

/// Sleep until `deadline`, or forever when no target has one.
async fn sleep_until_or_forever(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// One turn: assess reachability and, only when unreachable, redial.
async fn turn(port: Arc<dyn BootstrapPort>, target: Arc<ValidatedSeedPeer>) -> (Did, TurnOutcome) {
    let did = target.did();
    if port.reachable(target.as_ref()).await {
        tracing::debug!(target = %did, "bootstrap target reachable");
        return (did, TurnOutcome::Reachable);
    }
    match port.dial(target.as_ref()).await {
        Ok(()) => {
            tracing::info!(target = %did, endpoint = %target.endpoint(), "bootstrap target redialed");
            (did, TurnOutcome::Reachable)
        }
        Err(DialFailure::InFlight) => {
            tracing::debug!(target = %did, "bootstrap redial deferred: handshake in flight");
            (did, TurnOutcome::Deferred)
        }
        Err(DialFailure::Failed(error)) => {
            tracing::warn!(target = %did, endpoint = %target.endpoint(), %error, "bootstrap redial failed");
            (did, TurnOutcome::DialFailed)
        }
    }
}
