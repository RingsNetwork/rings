//! Supervisor shell tests over a scripted port under tokio's paused clock, plus target
//! validation and lookup-ledger tests.
//!
//! Every timing assertion below is in paused virtual time: the runtime advances the clock only
//! when no task can make progress, so attempt instants are exact, not approximate.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rings_core::dht::Did;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::probe::LookupReportLedger;
use super::probe::EARLY_REPORT_CAPACITY;
use super::schedule::is_one_slow_delay_after;
use super::BootstrapConfig;
use super::BootstrapPort;
use super::BootstrapSupervisor;
use super::BootstrapTargets;
use super::ManagedTarget;
use super::TransportDrops;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::StopSource;
use crate::seed::SeedPeer;

/// Fixed jitter seed so slow-cadence instants replay identically.
const JITTER_SEED: u64 = 7;
/// DID of the node under test; never a target.
const LOCAL: u32 = 0xFFFF;

/// Scripted dial behaviour for one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DialScript {
    /// The handshake completes and the target becomes reachable.
    Succeed,
    /// The handshake fails.
    Fail,
    /// The handshake never returns; only abort ends it.
    Hang,
    /// The handshake completes after the given virtual milliseconds.
    SucceedAfter(u64),
}

/// One recorded port call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallKind {
    /// A reachability assessment.
    Probe,
    /// A dial.
    Dial,
}

/// One recorded port call with its virtual instant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Call {
    at_ms: u64,
    target: Did,
    kind: CallKind,
}

/// Scripted state: reachability per target, queued dial scripts, and the call log.
#[derive(Default)]
struct PortState {
    reachable: BTreeMap<Did, bool>,
    dials: BTreeMap<Did, VecDeque<DialScript>>,
    log: Vec<Call>,
}

/// A [`BootstrapPort`] driven entirely by the test.
struct ScriptedPort {
    origin: Instant,
    state: Mutex<PortState>,
    hanging: Arc<AtomicUsize>,
}

/// Counts a hanging dial while its future is alive, so abort is observable.
struct HangGuard(Arc<AtomicUsize>);

impl HangGuard {
    /// Register one hanging dial.
    fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for HangGuard {
    /// Unregister the hanging dial, also when its future is aborted.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ScriptedPort {
    /// A port whose log clock starts now.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: Instant::now(),
            state: Mutex::new(PortState::default()),
            hanging: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Virtual milliseconds since the port was created.
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Lock the scripted state.
    fn state(&self) -> std::sync::MutexGuard<'_, PortState> {
        self.state.lock().unwrap()
    }

    /// Set whether `target` currently answers as reachable.
    fn set_reachable(&self, target: Did, reachable: bool) {
        self.state().reachable.insert(target, reachable);
    }

    /// Queue dial scripts for `target`; an exhausted queue fails.
    fn script_dials(&self, target: Did, scripts: impl IntoIterator<Item = DialScript>) {
        self.state()
            .dials
            .entry(target)
            .or_default()
            .extend(scripts);
    }

    /// Instants of every dial of `target`, in order.
    fn dial_times(&self, target: Did) -> Vec<u64> {
        self.state()
            .log
            .iter()
            .filter(|call| call.target == target && call.kind == CallKind::Dial)
            .map(|call| call.at_ms)
            .collect()
    }

    /// Instants of every probe of `target`, in order.
    fn probe_times(&self, target: Did) -> Vec<u64> {
        self.state()
            .log
            .iter()
            .filter(|call| call.target == target && call.kind == CallKind::Probe)
            .map(|call| call.at_ms)
            .collect()
    }

    /// Dials whose future is currently alive and hanging.
    fn hanging(&self) -> usize {
        self.hanging.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl BootstrapPort for ScriptedPort {
    /// Answer from the scripted reachability table (unknown targets are unreachable) and log it.
    async fn reachable(&self, target: &ManagedTarget) -> bool {
        let at_ms = self.now_ms();
        let mut state = self.state();
        let reachable = state.reachable.get(&target.did()).copied().unwrap_or(false);
        state.log.push(Call {
            at_ms,
            target: target.did(),
            kind: CallKind::Probe,
        });
        reachable
    }

    /// Follow the next queued script for the target (an exhausted queue fails) and log it.
    async fn dial(&self, target: &ManagedTarget) -> Result<()> {
        let at_ms = self.now_ms();
        let script = {
            let mut state = self.state();
            let script = state
                .dials
                .get_mut(&target.did())
                .and_then(VecDeque::pop_front)
                .unwrap_or(DialScript::Fail);
            state.log.push(Call {
                at_ms,
                target: target.did(),
                kind: CallKind::Dial,
            });
            script
        };
        match script {
            DialScript::Succeed => {
                self.set_reachable(target.did(), true);
                Ok(())
            }
            DialScript::Fail => Err(Error::BootstrapHandshake("scripted failure".to_string())),
            DialScript::Hang => {
                let _guard = HangGuard::new(&self.hanging);
                std::future::pending().await
            }
            DialScript::SucceedAfter(delay_ms) => {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                self.set_reachable(target.did(), true);
                Ok(())
            }
        }
    }
}

/// A seed entry for the numbered peer.
fn peer(n: u32) -> SeedPeer {
    SeedPeer {
        did: Did::from(n).to_string(),
        url: format!("https://peer{n}.example.com:50001/"),
        api_token: None,
    }
}

/// Validated targets for `peers`, for a local node that is none of them.
fn targets(peers: &[SeedPeer]) -> BootstrapTargets {
    BootstrapTargets::from_config(
        &BootstrapConfig {
            peers: peers.to_vec(),
        },
        Did::from(LOCAL),
    )
    .expect("test targets must validate")
}

/// A running supervisor over a scripted port.
struct Running {
    port: Arc<ScriptedPort>,
    drops: Arc<TransportDrops>,
    stop: StopSource,
    task: JoinHandle<()>,
}

impl Running {
    /// Spawn a supervisor over `peers` with `port` already scripted.
    fn spawn(port: Arc<ScriptedPort>, peers: &[SeedPeer]) -> Self {
        let targets = targets(peers);
        let evidence = super::ReachabilityEvidence::new(&targets);
        let drops = evidence.drops();
        let stop = StopSource::new();
        let supervisor =
            BootstrapSupervisor::new(targets, port.clone(), drops.clone(), JITTER_SEED);
        let task = tokio::spawn(supervisor.run(stop.token()));
        Self {
            port,
            drops,
            stop,
            task,
        }
    }

    /// Report the loss of the transport to `target` and make it unreachable.
    fn drop_transport(&self, target: Did) {
        self.port.set_reachable(target, false);
        self.drops
            .observe(target)
            .expect("drops must accept a loss");
    }

    /// Request stop and wait for the run to return.
    async fn shutdown(self) {
        self.stop.request_stop();
        tokio::time::timeout(Duration::from_secs(1), self.task)
            .await
            .expect("run must return promptly after stop")
            .expect("run task must not panic");
    }
}

/// An unreachable target from start-up is dialed at 0, 2, 4, 6, 8 s and then once per slow delay.
#[tokio::test(start_paused = true)]
async fn initial_failure_bursts_then_falls_back_to_the_slow_cadence() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    let running = Running::spawn(port.clone(), &[peer(1)]);

    tokio::time::sleep(Duration::from_millis(9_000)).await;
    assert_eq!(port.dial_times(target), vec![0, 2_000, 4_000, 6_000, 8_000]);

    tokio::time::sleep(Duration::from_millis(340_000)).await;
    let dials = port.dial_times(target);
    assert_eq!(dials.len(), 6, "exactly one slow attempt follows the burst");
    assert!(
        is_one_slow_delay_after(8_000, dials[5]),
        "sixth dial at {}",
        dials[5]
    );

    tokio::time::sleep(Duration::from_millis(340_000)).await;
    let dials = port.dial_times(target);
    assert_eq!(dials.len(), 7);
    assert!(is_one_slow_delay_after(dials[5], dials[6]));
    running.shutdown().await;
}

/// A successful dial ends the burst; the target is then only probed, one slow delay later.
#[tokio::test(start_paused = true)]
async fn recovery_resets_the_burst_and_rechecks_without_dialing() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [
        DialScript::Fail,
        DialScript::Fail,
        DialScript::Succeed,
    ]);
    let running = Running::spawn(port.clone(), &[peer(1)]);

    tokio::time::sleep(Duration::from_millis(400_000)).await;
    assert_eq!(port.dial_times(target), vec![0, 2_000, 4_000]);
    let probes = port.probe_times(target);
    assert_eq!(
        probes.len(),
        4,
        "three failing turns plus one periodic recheck"
    );
    assert!(
        is_one_slow_delay_after(4_000, probes[3]),
        "recheck at {}",
        probes[3]
    );
    running.shutdown().await;
}

/// A terminal transport state triggers an immediate probe and dial; a second loss restarts from the short delay.
#[tokio::test(start_paused = true)]
async fn a_transport_drop_reassesses_at_once_and_a_second_drop_restarts_the_burst() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.set_reachable(target, true);
    let running = Running::spawn(port.clone(), &[peer(1)]);

    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert!(port.dial_times(target).is_empty());
    running.drop_transport(target);
    port.script_dials(target, [
        DialScript::Fail,
        DialScript::Fail,
        DialScript::Succeed,
    ]);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(port.dial_times(target), vec![10_000, 12_000, 14_000]);

    tokio::time::sleep(Duration::from_millis(80_000)).await;
    running.drop_transport(target);
    port.script_dials(target, [DialScript::Fail, DialScript::Succeed]);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(
        port.dial_times(target),
        vec![10_000, 12_000, 14_000, 100_000, 102_000],
        "the second loss starts a fresh burst from the short delay"
    );
    running.shutdown().await;
}

/// A loss of a peer outside the target set causes no reassessment.
#[tokio::test(start_paused = true)]
async fn an_unmanaged_loss_is_ignored() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.set_reachable(target, true);
    let running = Running::spawn(port.clone(), &[peer(1)]);

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    running
        .drops
        .observe(Did::from(2))
        .expect("drops must accept any peer");
    tokio::time::sleep(Duration::from_millis(100_000)).await;
    assert_eq!(
        port.probe_times(target),
        vec![0],
        "no reassessment before the recheck"
    );
    assert!(port.dial_times(target).is_empty());
    running.shutdown().await;
}

/// A target whose dial hangs is never dialed again while busy, and does not delay another target's burst.
#[tokio::test(start_paused = true)]
async fn targets_retry_independently_and_never_overlap() {
    let hanging = Did::from(1);
    let failing = Did::from(2);
    let port = ScriptedPort::new();
    port.script_dials(hanging, [DialScript::Hang]);
    let running = Running::spawn(port.clone(), &[peer(1), peer(2)]);

    tokio::time::sleep(Duration::from_millis(9_000)).await;
    assert_eq!(
        port.dial_times(hanging),
        vec![0],
        "a hanging dial is never overlapped"
    );
    assert_eq!(port.dial_times(failing), vec![
        0, 2_000, 4_000, 6_000, 8_000
    ]);
    assert_eq!(port.hanging(), 1);
    running.shutdown().await;
}

/// Requesting stop returns the run promptly and drops the hanging dial future.
#[tokio::test(start_paused = true)]
async fn shutdown_aborts_an_in_flight_dial() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [DialScript::Hang]);
    let running = Running::spawn(port.clone(), &[peer(1)]);

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(port.hanging(), 1);
    running.shutdown().await;
    tokio::task::yield_now().await;
    assert_eq!(port.hanging(), 0, "dropping the run aborts its turns");
}

/// A drop that races a slow dial is reassessed the instant that dial settles, not one slow delay later.
#[tokio::test(start_paused = true)]
async fn a_drop_during_a_turn_is_reassessed_as_soon_as_the_turn_settles() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [DialScript::SucceedAfter(5_000)]);
    let running = Running::spawn(port.clone(), &[peer(1)]);
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    // The drop lands while the slow dial is in flight: it must neither overlap the busy turn
    // nor wait for the slow recheck once that turn settles as reachable.
    running.drop_transport(target);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(port.dial_times(target), vec![0]);
    assert_eq!(
        port.probe_times(target),
        vec![0, 5_000],
        "reassessed at the instant the raced turn settled"
    );
    running.shutdown().await;
}

/// Target validation accepts distinct public peers and rejects bad DIDs, non-public URLs, duplicates and self.
#[test]
fn targets_validate_dids_urls_duplicates_and_self() {
    let local = Did::from(LOCAL);
    let valid = BootstrapTargets::from_config(
        &BootstrapConfig {
            peers: vec![peer(1), peer(2)],
        },
        local,
    )
    .expect("distinct public peers validate");
    assert_eq!(valid.len(), 2);
    assert_eq!(valid.dids().into_iter().collect::<Vec<_>>(), vec![
        Did::from(1),
        Did::from(2)
    ]);

    let rejected = |peers: Vec<SeedPeer>| {
        BootstrapTargets::from_config(&BootstrapConfig { peers }, local)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    };
    assert!(rejected(vec![SeedPeer {
        did: "not-a-did".to_string(),
        ..peer(1)
    }])
    .contains("not a DID"));
    assert!(rejected(vec![SeedPeer {
        url: "http://127.0.0.1:50001/".to_string(),
        ..peer(1)
    }])
    .contains("Unsafe"));
    assert!(rejected(vec![
        SeedPeer {
            url: "https://other.example.com/".to_string(),
            ..peer(1)
        },
        peer(1)
    ])
    .contains("differing endpoints"));
    assert!(rejected(vec![peer(LOCAL)]).contains("itself"));

    let merged = BootstrapTargets::from_config(
        &BootstrapConfig {
            peers: vec![peer(1), peer(2), peer(1)],
        },
        local,
    )
    .expect("a verbatim repeat is merged");
    assert_eq!(merged.len(), 2);
    assert!(
        BootstrapTargets::from_config(&BootstrapConfig::default(), local)
            .expect("an empty section validates")
            .is_empty()
    );
}

/// The `Debug` rendering of a target never contains its bearer token.
#[test]
fn managed_target_debug_redacts_the_token() {
    let target = ManagedTarget::try_from(&SeedPeer {
        api_token: Some("0123456789abcdef".to_string()),
        ..peer(1)
    })
    .expect("a token-bearing peer validates");
    let rendered = format!("{target:?}");
    assert!(!rendered.contains("0123456789abcdef"));
    assert!(rendered.contains("[REDACTED]"));
    assert_eq!(target.api_token(), Some("0123456789abcdef"));
}

/// The ledger resolves a waiter whether the report arrived before or after it, and `forget` closes a waiter.
#[tokio::test]
async fn ledger_resolves_in_either_order_and_forgets() {
    let ledger = LookupReportLedger::default();
    let early = uuid::Uuid::new_v4();
    let late = uuid::Uuid::new_v4();
    let forgotten = uuid::Uuid::new_v4();

    ledger.observe(early, Did::from(1)).unwrap();
    let waiter = ledger.await_report(early).unwrap();
    assert_eq!(waiter.await, Ok(Did::from(1)));

    let waiter = ledger.await_report(late).unwrap();
    ledger.observe(late, Did::from(2)).unwrap();
    assert_eq!(waiter.await, Ok(Did::from(2)));

    let waiter = ledger.await_report(forgotten).unwrap();
    ledger.forget(forgotten).unwrap();
    assert!(waiter.await.is_err(), "a forgotten waiter is closed");
    assert_eq!(ledger.len().unwrap(), 0);
}

/// Early reports are bounded by `EARLY_REPORT_CAPACITY`, evicting the oldest first.
#[tokio::test]
async fn ledger_bounds_early_reports_by_evicting_the_oldest() {
    let ledger = LookupReportLedger::default();
    let oldest = uuid::Uuid::new_v4();
    ledger.observe(oldest, Did::from(1)).unwrap();
    for _ in 0..EARLY_REPORT_CAPACITY {
        ledger.observe(uuid::Uuid::new_v4(), Did::from(2)).unwrap();
    }
    assert_eq!(ledger.len().unwrap(), EARLY_REPORT_CAPACITY);
    let waiter = ledger.await_report(oldest).unwrap();
    ledger.forget(oldest).unwrap();
    assert!(waiter.await.is_err(), "the evicted report is gone");
}
