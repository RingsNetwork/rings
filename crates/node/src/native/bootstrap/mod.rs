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
//! - [`BootstrapPort`] — the two effects a turn needs: assess reachability and dial.
//! - [`BootstrapSupervisor`] — the shell: wakes on deadlines, transport drops and finished
//!   turns; runs at most one turn per target; exits on the run's stop token.
//! - [`BootstrapObserver`] — the swarm-side feed: lookup reports for the probe and connection
//!   state changes for prompt reassessment, both delivered by the [`Backend`] through
//!   [`BackendObserver`].
//!
//! ```text
//!                    ┌────────────────────────── run loop ──────────────────────────┐
//!  StopToken ──stop─▶│ drain drops ─▶ notice_drop ─▶ ∀ due target: begin ─▶ spawn turn│
//!  Signals ────wake─▶│ wait { stop | wake | turn finished | next deadline } ─▶ settle │
//!                    └──────────────────────────────────────────────────────────────┘
//!
//!  turn(t) := reachable(t) ? Reachable : (dial(t) = Ok ? Reachable : DialFailed)
//! ```
//!
//! Shutdown: the loop returns on the first stop observation; dropping its task set aborts any
//! in-flight turn, so no probe or handshake outlives the run. Transient failures stay inside
//! the loop; only configuration is validated up front and fails fast.
//!
//! [`Backend`]: crate::extension::Backend
//! [`BackendObserver`]: crate::extension::BackendObserver

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_transport::core::transport::WebrtcConnectionState;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinError;
use tokio::task::JoinSet;
use tokio::time::Instant;

use self::probe::LookupReportLedger;
use self::probe::ProcessorPort;
use self::schedule::BootstrapSchedule;
use self::schedule::TargetIndex;
use self::schedule::TurnOutcome;
use crate::error::Error;
use crate::error::Result;
use crate::extension::BackendObserver;
use crate::prelude::StopToken;
use crate::processor::Processor;
use crate::rpc_impl::validate_remote_rpc_url;
use crate::seed::SeedPeer;
use crate::sync_lock::lock;

pub mod probe;
mod schedule;
#[cfg(test)]
mod tests;

/// The `bootstrap` section of the native config: targets `rings run` keeps reachable.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BootstrapConfig {
    /// Managed targets, in the same shape as the entries of a seed document.
    #[serde(default)]
    pub peers: Vec<SeedPeer>,
}

/// One validated managed target: a parsed DID and a public HTTP(S) handshake endpoint.
#[derive(Clone)]
pub struct ManagedTarget {
    did: Did,
    url: String,
    api_token: Option<String>,
}

impl ManagedTarget {
    /// DID the endpoint must answer as.
    pub fn did(&self) -> Did {
        self.did
    }

    /// Validated handshake endpoint.
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    /// Bearer token for a peer that gates its handshake, never logged.
    pub(crate) fn api_token(&self) -> Option<&str> {
        self.api_token.as_deref()
    }
}

impl fmt::Debug for ManagedTarget {
    /// Render every field but the bearer token, which is replaced by `[REDACTED]`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedTarget")
            .field("did", &self.did)
            .field("url", &self.url)
            .field("api_token", &self.api_token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl TryFrom<&SeedPeer> for ManagedTarget {
    type Error = Error;

    /// Parse the DID and apply the remote RPC endpoint policy to the URL.
    fn try_from(peer: &SeedPeer) -> Result<Self> {
        let did = Did::from_str(peer.did.as_str()).map_err(|_| {
            Error::InvalidConfig(format!("bootstrap peer did is not a DID: {}", peer.did))
        })?;
        validate_remote_rpc_url(peer.url.as_str())?;
        Ok(Self {
            did,
            url: peer.url.clone(),
            api_token: peer.api_token.clone(),
        })
    }
}

/// Validated, duplicate-free managed targets that exclude the local node.
#[derive(Clone, Debug, Default)]
pub struct BootstrapTargets(Vec<ManagedTarget>);

impl BootstrapTargets {
    /// Validate `config` for the node `local`: every DID parses, every URL passes the remote
    /// RPC endpoint policy, no DID repeats, and none is `local` itself.
    pub fn from_config(config: &BootstrapConfig, local: Did) -> Result<Self> {
        let mut targets: Vec<ManagedTarget> = Vec::with_capacity(config.peers.len());
        for peer in &config.peers {
            let target = ManagedTarget::try_from(peer)?;
            if target.did == local {
                return Err(Error::InvalidConfig(format!(
                    "bootstrap peer {} is this node itself",
                    target.did
                )));
            }
            if targets.iter().any(|known| known.did == target.did) {
                return Err(Error::InvalidConfig(format!(
                    "bootstrap peer {} is listed twice",
                    target.did
                )));
            }
            targets.push(target);
        }
        Ok(Self(targets))
    }

    /// Whether there is nothing to supervise.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of managed targets.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// DIDs of every target, the filter for transport-drop signals.
    pub fn dids(&self) -> BTreeSet<Did> {
        self.0.iter().map(ManagedTarget::did).collect()
    }
}

/// Effects one supervisor turn performs on a target.
#[async_trait]
pub trait BootstrapPort: Send + Sync + 'static {
    /// Whether `target` is reachable through the overlay right now.
    async fn reachable(&self, target: &ManagedTarget) -> bool;

    /// Redial `target` through its HTTP endpoint; `Ok` once the handshake is accepted.
    async fn dial(&self, target: &ManagedTarget) -> Result<()>;
}

/// Transport-drop signals from the swarm to the supervisor, bounded by the target set.
pub struct BootstrapSignals {
    targets: BTreeSet<Did>,
    dropped: Mutex<BTreeSet<Did>>,
    wake: Notify,
}

impl BootstrapSignals {
    /// Signals that record drops for `targets` only.
    fn new(targets: BTreeSet<Did>) -> Self {
        Self {
            targets,
            dropped: Mutex::new(BTreeSet::new()),
            wake: Notify::new(),
        }
    }

    /// Record a terminal transport state for `peer` when it is a managed target, waking the
    /// supervisor. Other peers and non-terminal states are ignored.
    pub fn observe(&self, peer: Did, state: WebrtcConnectionState) -> Result<()> {
        if !is_terminal_transport_state(state) || !self.targets.contains(&peer) {
            return Ok(());
        }
        lock(&self.dropped)?.insert(peer);
        self.wake.notify_one();
        Ok(())
    }

    /// Take every drop recorded since the previous call.
    fn take_dropped(&self) -> Result<BTreeSet<Did>> {
        Ok(std::mem::take(&mut *lock(&self.dropped)?))
    }
}

/// Whether `state` is a terminal WebRTC state, after which the core leaves the peer's DHT
/// entry. `Disconnected` is transient ICE and often recovers, so it is not one.
const fn is_terminal_transport_state(state: WebrtcConnectionState) -> bool {
    matches!(
        state,
        WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
    )
}

/// Swarm-side feed for the supervisor: lookup reports and transport drops.
pub struct BootstrapObserver {
    reports: Arc<LookupReportLedger>,
    signals: Arc<BootstrapSignals>,
}

impl BootstrapObserver {
    /// An observer that forwards drops of `targets` and every lookup report.
    pub fn new(targets: BTreeSet<Did>) -> Self {
        Self {
            reports: Arc::new(LookupReportLedger::default()),
            signals: Arc::new(BootstrapSignals::new(targets)),
        }
    }

    /// Shared lookup-report ledger.
    pub fn reports(&self) -> Arc<LookupReportLedger> {
        self.reports.clone()
    }

    /// Shared transport-drop signals.
    pub fn signals(&self) -> Arc<BootstrapSignals> {
        self.signals.clone()
    }
}

impl BackendObserver for BootstrapObserver {
    /// Hand the report to the ledger; a poisoned ledger is logged, never propagated.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did) {
        if let Err(error) = self.reports.observe(tx_id, successor) {
            tracing::error!(%tx_id, %error, "bootstrap lookup ledger unavailable");
        }
    }

    /// Hand the transition to the drop signals; a poisoned signal set is logged, never propagated.
    fn connection_state(&self, peer: Did, state: WebrtcConnectionState) {
        if let Err(error) = self.signals.observe(peer, state) {
            tracing::error!(%peer, %error, "bootstrap drop signals unavailable");
        }
    }
}

/// The run-owned supervisor; see the module diagram.
pub struct BootstrapSupervisor<P> {
    targets: Vec<Arc<ManagedTarget>>,
    port: Arc<P>,
    signals: Arc<BootstrapSignals>,
    schedule: BootstrapSchedule,
    origin: Instant,
}

/// What woke the run loop.
enum Wake {
    /// The run's stop token was observed.
    Stop,
    /// A transport drop was recorded.
    Signal,
    /// A turn finished, keyed by its task id.
    Turn(std::result::Result<(tokio::task::Id, (TargetIndex, TurnOutcome)), JoinError>),
    /// The next scheduled deadline passed.
    Deadline,
}

impl BootstrapSupervisor<ProcessorPort> {
    /// Supervisor over a live processor, or `None` when there is nothing to supervise.
    pub fn over_processor(
        targets: BootstrapTargets,
        processor: Arc<Processor>,
        observer: &BootstrapObserver,
    ) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        let port = Arc::new(ProcessorPort::new(processor, observer.reports()));
        Some(Self::new(targets, port, observer.signals(), rand::random()))
    }
}

impl<P: BootstrapPort> BootstrapSupervisor<P> {
    /// Supervisor over `port` with every target due immediately and jitter seeded by
    /// `jitter_seed`.
    pub fn new(
        targets: BootstrapTargets,
        port: Arc<P>,
        signals: Arc<BootstrapSignals>,
        jitter_seed: u64,
    ) -> Self {
        let schedule = BootstrapSchedule::new(targets.len(), jitter_seed);
        Self {
            targets: targets.0.into_iter().map(Arc::new).collect(),
            port,
            signals,
            schedule,
            origin: Instant::now(),
        }
    }

    /// Run until `stop` is observed; never returns otherwise.
    pub async fn run(mut self, stop: StopToken) {
        let mut turns: JoinSet<(TargetIndex, TurnOutcome)> = JoinSet::new();
        let mut inflight: HashMap<tokio::task::Id, TargetIndex> = HashMap::new();
        loop {
            self.drain_drops();
            self.start_due_turns(&mut turns, &mut inflight);
            let deadline = self.schedule.next_deadline_ms().and_then(|deadline_ms| {
                self.origin.checked_add(Duration::from_millis(deadline_ms))
            });
            match wait_for_wake(&stop, self.signals.as_ref(), &mut turns, deadline).await {
                Wake::Stop => return,
                Wake::Signal | Wake::Deadline => {}
                Wake::Turn(Ok((id, (index, outcome)))) => {
                    inflight.remove(&id);
                    self.schedule.settle(index, outcome, self.now_ms());
                }
                Wake::Turn(Err(error)) => {
                    // A turn neither panics (denied by lints) nor is aborted before shutdown,
                    // so this is unreachable in practice; settle as a failure to stay total.
                    if let Some(index) = inflight.remove(&error.id()) {
                        tracing::error!(%error, "bootstrap turn did not complete");
                        self.schedule
                            .settle(index, TurnOutcome::DialFailed, self.now_ms());
                    }
                }
            }
        }
    }

    /// Milliseconds since the supervisor started, saturating.
    fn now_ms(&self) -> u64 {
        schedule::duration_ms(self.origin.elapsed())
    }

    /// Fold every recorded transport drop into the schedule.
    fn drain_drops(&mut self) {
        let dropped = match self.signals.take_dropped() {
            Ok(dropped) => dropped,
            Err(error) => {
                tracing::error!(%error, "bootstrap drop signals unavailable");
                return;
            }
        };
        let now_ms = self.now_ms();
        for peer in dropped {
            let Some(index) = self.targets.iter().position(|target| target.did == peer) else {
                continue;
            };
            if self.schedule.notice_drop(index, now_ms) {
                tracing::info!(target = %peer, "bootstrap target transport lost; reassessing");
            }
        }
    }

    /// Spawn one turn for every target that is due.
    fn start_due_turns(
        &mut self,
        turns: &mut JoinSet<(TargetIndex, TurnOutcome)>,
        inflight: &mut HashMap<tokio::task::Id, TargetIndex>,
    ) {
        let due: Vec<TargetIndex> = self.schedule.due(self.now_ms()).collect();
        for index in due {
            let Some(target) = self.targets.get(index) else {
                continue;
            };
            if !self.schedule.begin(index) {
                continue;
            }
            let handle = turns.spawn(turn(self.port.clone(), target.clone(), index));
            inflight.insert(handle.id(), index);
        }
    }
}

/// Block until the run should act again.
///
/// An empty task set disables its branch rather than completing, so an idle supervisor sleeps
/// until its deadline, a drop signal, or the stop token.
async fn wait_for_wake(
    stop: &StopToken,
    signals: &BootstrapSignals,
    turns: &mut JoinSet<(TargetIndex, TurnOutcome)>,
    deadline: Option<Instant>,
) -> Wake {
    tokio::select! {
        _ = stop.stopped() => Wake::Stop,
        _ = signals.wake.notified() => Wake::Signal,
        Some(joined) = turns.join_next_with_id() => Wake::Turn(joined),
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
async fn turn<P: BootstrapPort>(
    port: Arc<P>,
    target: Arc<ManagedTarget>,
    index: TargetIndex,
) -> (TargetIndex, TurnOutcome) {
    if port.reachable(target.as_ref()).await {
        tracing::debug!(target = %target.did, "bootstrap target reachable");
        return (index, TurnOutcome::Reachable);
    }
    match port.dial(target.as_ref()).await {
        Ok(()) => {
            tracing::info!(target = %target.did, url = %target.url, "bootstrap target redialed");
            (index, TurnOutcome::Reachable)
        }
        Err(error) => {
            tracing::warn!(
                target = %target.did,
                url = %target.url,
                %error,
                "bootstrap redial failed"
            );
            (index, TurnOutcome::DialFailed)
        }
    }
}
