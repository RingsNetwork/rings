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
//! - [`BootstrapPort`] — the two effects a turn needs: assess reachability and dial;
//!   `ProcessorPort` is the production port over a live processor.
//! - [`BootstrapSupervisor`] — the shell: wakes on deadlines, transport drops and finished
//!   turns; runs at most one turn per target; exits on the run's stop token.
//! - [`BootstrapObserver`] — the swarm-side feed: lookup reports for the probe and transport
//!   drops for prompt reassessment, both delivered by the [`Backend`] through
//!   [`BackendObserver`].
//!
//! ```text
//!                    ┌────────────────────────── run loop ──────────────────────────┐
//!  StopToken ──stop─▶│ drain drops ─▶ notice_drop ─▶ ∀ due target: begin ─▶ spawn turn│
//!  Drops ──────wake─▶│ wait { stop | wake | turn finished | next deadline } ─▶ settle │
//!                    └──────────────────────────────────────────────────────────────┘
//!
//!  turn(t) := reachable(t) ? Reachable : (dial(t) = Ok ? Reachable : DialFailed)
//! ```
//!
//! Shutdown: the loop returns on the first stop observation; dropping its turns aborts any
//! in-flight probe or handshake, so none outlives the run. Transient failures stay inside the
//! loop; only configuration is validated up front and fails fast.
//!
//! [`Backend`]: crate::extension::Backend
//! [`BackendObserver`]: crate::extension::BackendObserver

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
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
use tokio::task::JoinSet;
use tokio::time::Instant;

use self::probe::LookupReportLedger;
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

mod probe;
mod schedule;
#[cfg(test)]
mod tests;

pub use self::probe::ProcessorPort;

/// The `bootstrap` section of the native config: targets `rings run` keeps reachable.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BootstrapConfig {
    /// Managed targets, in the same shape as the entries of a seed document.
    #[serde(default)]
    pub peers: Vec<SeedPeer>,
}

/// One validated managed target: a parsed DID and a public HTTP(S) handshake endpoint.
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

/// Validated managed targets: one per DID, none of them the local node.
#[derive(Debug, Default)]
pub struct BootstrapTargets(Vec<ManagedTarget>);

impl BootstrapTargets {
    /// Validate `config` for the node `local`: every DID parses, every URL passes the remote
    /// RPC endpoint policy, and none is `local` itself. An entry repeated verbatim (as when the
    /// same seed document feeds both the config and `--bootstrap-seed`) is merged; a DID listed
    /// with a different endpoint or token is rejected as ambiguous.
    pub fn from_config(config: &BootstrapConfig, local: Did) -> Result<Self> {
        let mut seen: Vec<&SeedPeer> = Vec::with_capacity(config.peers.len());
        let mut targets: Vec<ManagedTarget> = Vec::with_capacity(config.peers.len());
        for peer in &config.peers {
            if seen.contains(&peer) {
                continue;
            }
            let target = ManagedTarget::try_from(peer)?;
            if target.did == local {
                return Err(Error::InvalidConfig(format!(
                    "bootstrap peer {} is this node itself",
                    target.did
                )));
            }
            if targets.iter().any(|known| known.did == target.did) {
                return Err(Error::InvalidConfig(format!(
                    "bootstrap peer {} is listed with differing endpoints",
                    target.did
                )));
            }
            seen.push(peer);
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
    pub(crate) fn dids(&self) -> BTreeSet<Did> {
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

/// Terminal transport losses of managed targets, recorded by the swarm callback and drained by
/// the supervisor. Bounded by the target set.
pub(crate) struct TransportDrops {
    targets: BTreeSet<Did>,
    dropped: Mutex<BTreeSet<Did>>,
    wake: Notify,
}

impl TransportDrops {
    /// Drops recorded for `targets` only.
    fn new(targets: BTreeSet<Did>) -> Self {
        Self {
            targets,
            dropped: Mutex::new(BTreeSet::new()),
            wake: Notify::new(),
        }
    }

    /// Record `state` for `peer` when it is terminal and `peer` is a managed target, waking the
    /// supervisor. Other peers and non-terminal states (`Disconnected` is transient ICE that
    /// often recovers, and the core keeps the peer's DHT entry through it) are ignored.
    pub(crate) fn observe(&self, peer: Did, state: WebrtcConnectionState) -> Result<()> {
        if !state.is_terminal() || !self.targets.contains(&peer) {
            return Ok(());
        }
        lock(&self.dropped)?.insert(peer);
        self.wake.notify_one();
        Ok(())
    }

    /// Take every drop recorded since the previous call.
    fn take(&self) -> Result<BTreeSet<Did>> {
        Ok(std::mem::take(&mut *lock(&self.dropped)?))
    }
}

/// Swarm-side feed for the supervisor: lookup reports and transport drops.
pub struct BootstrapObserver {
    reports: Arc<LookupReportLedger>,
    drops: Arc<TransportDrops>,
}

impl BootstrapObserver {
    /// An observer that records drops of `targets` and every lookup report.
    pub fn new(targets: &BootstrapTargets) -> Self {
        Self {
            reports: Arc::new(LookupReportLedger::default()),
            drops: Arc::new(TransportDrops::new(targets.dids())),
        }
    }

    /// Shared lookup-report ledger.
    pub(crate) fn reports(&self) -> Arc<LookupReportLedger> {
        self.reports.clone()
    }

    /// Shared transport drops.
    pub(crate) fn drops(&self) -> Arc<TransportDrops> {
        self.drops.clone()
    }
}

impl BackendObserver for BootstrapObserver {
    /// Hand the report to the ledger; a poisoned ledger is logged, never propagated.
    fn lookup_report(&self, tx_id: uuid::Uuid, successor: Did) {
        if let Err(error) = self.reports.observe(tx_id, successor) {
            tracing::error!(%tx_id, %error, "bootstrap lookup ledger unavailable");
        }
    }

    /// Hand the transition to the drop record; a poisoned record is logged, never propagated.
    fn connection_state(&self, peer: Did, state: WebrtcConnectionState) {
        if let Err(error) = self.drops.observe(peer, state) {
            tracing::error!(%peer, %error, "bootstrap drop record unavailable");
        }
    }
}

/// The run-owned supervisor; see the module diagram.
pub struct BootstrapSupervisor<P> {
    targets: Vec<Arc<ManagedTarget>>,
    port: Arc<P>,
    drops: Arc<TransportDrops>,
    schedule: BootstrapSchedule,
    origin: Instant,
}

/// In-flight turns: a task set plus the target each task serves.
///
/// Invariant: `keys(targets) = ids(tasks)`, so a task that ends without a result (aborted or
/// panicked) still resolves to its target and settles as a failed dial.
#[derive(Default)]
struct Turns {
    tasks: JoinSet<TurnOutcome>,
    targets: HashMap<tokio::task::Id, TargetIndex>,
}

impl Turns {
    /// Spawn `turn` on behalf of `index`.
    fn spawn(
        &mut self,
        index: TargetIndex,
        turn: impl Future<Output = TurnOutcome> + Send + 'static,
    ) {
        let handle = self.tasks.spawn(turn);
        self.targets.insert(handle.id(), index);
    }

    /// The next finished turn, or `None` once none is in flight (the caller's select disables
    /// this branch rather than completing it). A task the map does not know is skipped rather
    /// than allowed to stall the branch.
    async fn next(&mut self) -> Option<(TargetIndex, TurnOutcome)> {
        loop {
            let (id, outcome) = match self.tasks.join_next_with_id().await? {
                Ok((id, outcome)) => (id, outcome),
                Err(error) => {
                    tracing::error!(%error, "bootstrap turn did not complete");
                    (error.id(), TurnOutcome::DialFailed)
                }
            };
            if let Some(index) = self.targets.remove(&id) {
                return Some((index, outcome));
            }
        }
    }
}

/// What woke the run loop.
enum Wake {
    /// The run's stop token was observed.
    Stop,
    /// A transport drop was recorded.
    Drop,
    /// A turn finished for the target.
    Turn(TargetIndex, TurnOutcome),
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
        Some(Self::new(targets, port, observer.drops(), rand::random()))
    }
}

impl<P: BootstrapPort> BootstrapSupervisor<P> {
    /// Supervisor over `port` with every target due immediately and jitter seeded by
    /// `jitter_seed`.
    pub(crate) fn new(
        targets: BootstrapTargets,
        port: Arc<P>,
        drops: Arc<TransportDrops>,
        jitter_seed: u64,
    ) -> Self {
        let schedule = BootstrapSchedule::new(targets.len(), jitter_seed);
        Self {
            targets: targets.0.into_iter().map(Arc::new).collect(),
            port,
            drops,
            schedule,
            origin: Instant::now(),
        }
    }

    /// Run until `stop` is observed; never returns otherwise.
    pub async fn run(mut self, stop: StopToken) {
        let mut turns = Turns::default();
        loop {
            self.drain_drops();
            self.start_due_turns(&mut turns);
            let deadline = self.schedule.next_deadline_ms().and_then(|deadline_ms| {
                self.origin.checked_add(Duration::from_millis(deadline_ms))
            });
            match wait_for_wake(&stop, self.drops.as_ref(), &mut turns, deadline).await {
                Wake::Stop => return,
                Wake::Drop | Wake::Deadline => {}
                Wake::Turn(index, outcome) => {
                    self.schedule.settle(index, outcome, self.now_ms());
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
        let dropped = match self.drops.take() {
            Ok(dropped) => dropped,
            Err(error) => {
                tracing::error!(%error, "bootstrap drop record unavailable");
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
    fn start_due_turns(&mut self, turns: &mut Turns) {
        let due: Vec<TargetIndex> = self.schedule.due(self.now_ms()).collect();
        for index in due {
            let Some(target) = self.targets.get(index) else {
                continue;
            };
            if !self.schedule.begin(index) {
                continue;
            }
            turns.spawn(index, turn(self.port.clone(), target.clone()));
        }
    }
}

/// Block until the run should act again.
async fn wait_for_wake(
    stop: &StopToken,
    drops: &TransportDrops,
    turns: &mut Turns,
    deadline: Option<Instant>,
) -> Wake {
    tokio::select! {
        _ = stop.stopped() => Wake::Stop,
        _ = drops.wake.notified() => Wake::Drop,
        Some((index, outcome)) = turns.next() => Wake::Turn(index, outcome),
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
async fn turn<P: BootstrapPort>(port: Arc<P>, target: Arc<ManagedTarget>) -> TurnOutcome {
    if port.reachable(target.as_ref()).await {
        tracing::debug!(target = %target.did, "bootstrap target reachable");
        return TurnOutcome::Reachable;
    }
    match port.dial(target.as_ref()).await {
        Ok(()) => {
            tracing::info!(target = %target.did, url = %target.url, "bootstrap target redialed");
            TurnOutcome::Reachable
        }
        Err(error) => {
            tracing::warn!(
                target = %target.did,
                url = %target.url,
                %error,
                "bootstrap redial failed"
            );
            TurnOutcome::DialFailed
        }
    }
}
