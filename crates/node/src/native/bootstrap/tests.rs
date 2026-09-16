//! Supervisor shell tests over a scripted port under tokio's paused clock, plus target
//! validation and evidence-record tests.
//!
//! Every timing assertion below is in paused virtual time: the runtime advances the clock only
//! when no task can make progress, so attempt instants are exact, not approximate.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use rings_core::dht::Did;
use tokio::sync::futures::Notified;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::evidence::ReachabilityEvidence;
use super::evidence::Rendezvous;
use super::evidence::EARLY_REPORT_CAPACITY;
use super::schedule::duration_ms;
use super::schedule::is_one_slow_delay_after;
use super::BootstrapPort;
use super::BootstrapSupervisor;
use super::BootstrapTargetError;
use super::BootstrapTargets;
use super::DialFailure;
use crate::error::Error;
use crate::native::config::BootstrapConfig;
use crate::prelude::StopSource;
use crate::seed::SeedPeer;
use crate::seed::SeedPeerError;
use crate::seed::ValidatedSeedPeer;

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
    /// The core reports a handshake to the target already in flight.
    InFlight,
    /// The handshake never returns; only cancellation ends it.
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

/// Scripted state: reachability per target, queued dial scripts, the call log, and the count
/// of dials currently hanging.
#[derive(Default)]
struct PortState {
    reachable: BTreeMap<Did, bool>,
    dials: BTreeMap<Did, VecDeque<DialScript>>,
    log: Vec<Call>,
    hanging: usize,
}

/// A [`BootstrapPort`] driven entirely by the test.
struct ScriptedPort {
    origin: Instant,
    state: Mutex<PortState>,
    hang_released: Notify,
}

/// Counts a hanging dial while its future is alive and announces its cancellation, so the
/// shutdown test waits for the event rather than for a scheduler pass.
struct HangGuard<'port> {
    port: &'port ScriptedPort,
}

impl<'port> HangGuard<'port> {
    /// Register one hanging dial.
    fn new(port: &'port ScriptedPort) -> Self {
        port.state().hanging += 1;
        Self { port }
    }
}

impl Drop for HangGuard<'_> {
    /// Unregister the hanging dial and announce it, also when its future is dropped.
    fn drop(&mut self) {
        self.port.state().hanging -= 1;
        self.port.hang_released.notify_one();
    }
}

impl ScriptedPort {
    /// A port whose log clock starts now.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: Instant::now(),
            state: Mutex::new(PortState::default()),
            hang_released: Notify::new(),
        })
    }

    /// Virtual milliseconds since the port was created.
    fn now_ms(&self) -> u64 {
        duration_ms(self.origin.elapsed())
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

    /// Instants of every call of `kind` on `target`, in order.
    fn call_times(&self, target: Did, kind: CallKind) -> Vec<u64> {
        self.state()
            .log
            .iter()
            .filter(|call| call.target == target && call.kind == kind)
            .map(|call| call.at_ms)
            .collect()
    }

    /// Dials whose future is currently alive and hanging.
    fn hanging(&self) -> usize {
        self.state().hanging
    }

    /// Resolves once a hanging dial's future is dropped.
    fn released(&self) -> Notified<'_> {
        self.hang_released.notified()
    }
}

#[async_trait::async_trait]
impl BootstrapPort for ScriptedPort {
    /// Answer from the scripted reachability table (unknown targets are unreachable) and log it.
    async fn reachable(&self, target: &ValidatedSeedPeer) -> bool {
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
    async fn dial(&self, target: &ValidatedSeedPeer) -> std::result::Result<(), DialFailure> {
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
            DialScript::Fail => Err(DialFailure::Failed(Error::AdmissionTimedOut {
                peer: target.did(),
            })),
            DialScript::InFlight => Err(DialFailure::InFlight),
            DialScript::Hang => {
                let _guard = HangGuard::new(self);
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
fn targets(peers: Vec<SeedPeer>) -> BootstrapTargets {
    BootstrapTargets::from_config(BootstrapConfig { peers }, Did::from(LOCAL))
        .expect("test targets must validate")
}

/// The target error `peers` are rejected with.
fn rejection(peers: Vec<SeedPeer>) -> Option<BootstrapTargetError> {
    match BootstrapTargets::from_config(BootstrapConfig { peers }, Did::from(LOCAL)) {
        Err(Error::BootstrapTarget(error)) => Some(error),
        _ => None,
    }
}

/// A running supervisor over a scripted port.
struct Running {
    port: Arc<ScriptedPort>,
    evidence: Arc<ReachabilityEvidence>,
    stop: StopSource,
    task: JoinHandle<()>,
}

impl Running {
    /// Spawn a supervisor over `peers` with `port` already scripted.
    fn spawn(port: Arc<ScriptedPort>, peers: Vec<SeedPeer>) -> Self {
        let evidence = Arc::new(ReachabilityEvidence::default());
        let stop = StopSource::new();
        let supervisor =
            BootstrapSupervisor::new(targets(peers), port.clone(), evidence.clone(), JITTER_SEED);
        let task = tokio::spawn(supervisor.run(stop.token()));
        Self {
            port,
            evidence,
            stop,
            task,
        }
    }

    /// Report that `target` left the local DHT and make it unreachable.
    fn lose(&self, target: Did) {
        self.port.set_reachable(target, false);
        self.evidence
            .losses()
            .observe(target)
            .expect("losses must accept a retirement");
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
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(9_000)).await;
    assert_eq!(port.call_times(target, CallKind::Dial), vec![
        0, 2_000, 4_000, 6_000, 8_000
    ]);

    tokio::time::sleep(Duration::from_millis(340_000)).await;
    let dials = port.call_times(target, CallKind::Dial);
    assert_eq!(dials.len(), 6, "exactly one slow attempt follows the burst");
    assert!(is_one_slow_delay_after(8_000, dials[5]));

    tokio::time::sleep(Duration::from_millis(340_000)).await;
    let dials = port.call_times(target, CallKind::Dial);
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
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(400_000)).await;
    assert_eq!(port.call_times(target, CallKind::Dial), vec![
        0, 2_000, 4_000
    ]);
    let probes = port.call_times(target, CallKind::Probe);
    assert_eq!(
        probes.len(),
        4,
        "three failing turns plus one periodic recheck"
    );
    assert!(is_one_slow_delay_after(4_000, probes[3]));
    running.shutdown().await;
}

/// A retirement triggers an immediate probe and dial; a second loss restarts from the short delay.
#[tokio::test(start_paused = true)]
async fn a_loss_reassesses_at_once_and_a_second_loss_restarts_the_burst() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.set_reachable(target, true);
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert!(port.call_times(target, CallKind::Dial).is_empty());
    running.lose(target);
    port.script_dials(target, [
        DialScript::Fail,
        DialScript::Fail,
        DialScript::Succeed,
    ]);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(port.call_times(target, CallKind::Dial), vec![
        10_000, 12_000, 14_000
    ]);

    tokio::time::sleep(Duration::from_millis(80_000)).await;
    running.lose(target);
    port.script_dials(target, [DialScript::Fail, DialScript::Succeed]);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(
        port.call_times(target, CallKind::Dial),
        vec![10_000, 12_000, 14_000, 100_000, 102_000],
        "the second loss starts a fresh burst from the short delay"
    );
    running.shutdown().await;
}

/// A loss recorded while a target waits in the slow cadence restarts its burst at once.
#[tokio::test(start_paused = true)]
async fn a_loss_wakes_a_target_sleeping_to_its_slow_deadline() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(20_000)).await;
    assert_eq!(port.call_times(target, CallKind::Dial).len(), 5);
    running.lose(target);
    tokio::time::sleep(Duration::from_millis(9_000)).await;
    assert_eq!(
        port.call_times(target, CallKind::Dial),
        vec![0, 2_000, 4_000, 6_000, 8_000, 20_000, 22_000, 24_000, 26_000, 28_000],
        "the slow wait is abandoned for a fresh burst"
    );
    running.shutdown().await;
}

/// A dial refused because a handshake is already in flight waits one short delay without
/// counting as a failure, so the burst is not consumed by the node's own attempt.
#[tokio::test(start_paused = true)]
async fn an_in_flight_handshake_defers_without_counting() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [
        DialScript::Fail,
        DialScript::Fail,
        DialScript::InFlight,
        DialScript::InFlight,
        DialScript::InFlight,
        DialScript::Fail,
        DialScript::Fail,
        DialScript::Fail,
    ]);
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(15_000)).await;
    let dials = port.call_times(target, CallKind::Dial);
    assert_eq!(
        dials,
        vec![0, 2_000, 4_000, 6_000, 8_000, 10_000, 12_000, 14_000],
        "deferrals keep the short delay"
    );
    tokio::time::sleep(Duration::from_millis(340_000)).await;
    let dials = port.call_times(target, CallKind::Dial);
    assert_eq!(dials.len(), 9);
    assert!(
        is_one_slow_delay_after(14_000, dials[8]),
        "the fifth counted failure, not the eighth call, enters the slow cadence"
    );
    running.shutdown().await;
}

/// A hanging dial is never dialed again while busy, and does not delay another target's burst.
#[tokio::test(start_paused = true)]
async fn targets_retry_independently_and_never_overlap() {
    let hanging = Did::from(1);
    let failing = Did::from(2);
    let port = ScriptedPort::new();
    port.script_dials(hanging, [DialScript::Hang]);
    let running = Running::spawn(port.clone(), vec![peer(1), peer(2)]);

    tokio::time::sleep(Duration::from_millis(9_000)).await;
    assert_eq!(port.call_times(hanging, CallKind::Dial), vec![0]);
    assert_eq!(port.call_times(failing, CallKind::Dial), vec![
        0, 2_000, 4_000, 6_000, 8_000
    ]);
    assert_eq!(port.hanging(), 1);
    running.shutdown().await;
}

/// Requesting stop returns the run promptly and drops the hanging dial future.
#[tokio::test(start_paused = true)]
async fn shutdown_cancels_an_in_flight_dial() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [DialScript::Hang]);
    let running = Running::spawn(port.clone(), vec![peer(1)]);

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(port.hanging(), 1);
    let released = port.released();
    running.shutdown().await;
    tokio::time::timeout(Duration::from_secs(1), released)
        .await
        .expect("dropping the run cancels its turns");
    assert_eq!(port.hanging(), 0);
}

/// A loss that races a slow dial is reassessed the instant that dial settles, not one slow
/// delay later.
#[tokio::test(start_paused = true)]
async fn a_loss_during_a_turn_is_reassessed_as_soon_as_the_turn_settles() {
    let target = Did::from(1);
    let port = ScriptedPort::new();
    port.script_dials(target, [DialScript::SucceedAfter(5_000)]);
    let running = Running::spawn(port.clone(), vec![peer(1)]);
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    running.lose(target);
    tokio::time::sleep(Duration::from_millis(10_000)).await;
    assert_eq!(port.call_times(target, CallKind::Dial), vec![0]);
    assert_eq!(
        port.call_times(target, CallKind::Probe),
        vec![0, 5_000],
        "reassessed at the instant the raced turn settled"
    );
    running.shutdown().await;
}

/// Target validation accepts distinct public peers and rejects, by typed reason, bad DIDs,
/// non-public URLs, one DID under two endpoints, one endpoint under two DIDs, and self; a
/// verbatim repeat merges.
#[test]
fn targets_validate_dids_urls_duplicates_and_self() {
    let local = Did::from(LOCAL);
    assert_eq!(targets(vec![peer(1), peer(2)]).len(), 2);

    assert_eq!(
        rejection(vec![SeedPeer {
            did: "not-a-did".to_string(),
            ..peer(1)
        }]),
        Some(BootstrapTargetError::Peer(SeedPeerError::NotADid(
            "not-a-did".to_string()
        )))
    );
    assert!(matches!(
        rejection(vec![SeedPeer {
            url: "http://127.0.0.1:50001/".to_string(),
            ..peer(1)
        }]),
        Some(BootstrapTargetError::Peer(SeedPeerError::UnusableEndpoint { did, .. }))
            if did == Did::from(1)
    ));
    assert_eq!(
        rejection(vec![
            SeedPeer {
                url: "https://other.example.com/".to_string(),
                ..peer(1)
            },
            peer(1),
        ]),
        Some(BootstrapTargetError::ConflictingEntries(Did::from(1)))
    );
    assert!(matches!(
        rejection(vec![peer(1), SeedPeer {
            did: Did::from(2).to_string(),
            ..peer(1)
        },]),
        Some(BootstrapTargetError::EndpointUnderTwoDids(_))
    ));
    assert_eq!(
        rejection(vec![peer(LOCAL)]),
        Some(BootstrapTargetError::LocalNode(local))
    );

    assert_eq!(
        targets(vec![peer(1), peer(2), peer(1)]).len(),
        2,
        "a verbatim repeat is merged"
    );
    assert!(
        BootstrapTargets::from_config(BootstrapConfig::default(), local)
            .expect("an empty section validates")
            .is_empty()
    );
}

/// A rendezvous resolves a waiter whether the value arrived before or after it, replaces an
/// earlier waiter for the same key, and `forget` closes a waiter.
#[tokio::test]
async fn rendezvous_resolves_in_either_order_replaces_and_forgets() {
    let rendezvous: Rendezvous<u8, Did> = Rendezvous::new(EARLY_REPORT_CAPACITY);

    rendezvous.observe(1, Did::from(1)).unwrap();
    let waiter = rendezvous.wait_for(1).unwrap();
    assert_eq!(waiter.await, Ok(Did::from(1)));

    let stale = rendezvous.wait_for(2).unwrap();
    let waiter = rendezvous.wait_for(2).unwrap();
    rendezvous.observe(2, Did::from(2)).unwrap();
    assert!(stale.await.is_err(), "the replaced waiter is closed");
    assert_eq!(waiter.await, Ok(Did::from(2)));

    let waiter = rendezvous.wait_for(3).unwrap();
    rendezvous.forget(3).unwrap();
    assert!(waiter.await.is_err(), "a forgotten waiter is closed");
    assert_eq!(rendezvous.len().unwrap(), 0);
}

/// Early values are bounded by the capacity, evicting the oldest first, and a capacity of
/// zero buffers nothing.
#[tokio::test]
async fn rendezvous_bounds_early_values() {
    let rendezvous: Rendezvous<u8, ()> = Rendezvous::new(2);
    for key in 1..=3 {
        rendezvous.observe(key, ()).unwrap();
    }
    assert_eq!(rendezvous.len().unwrap(), 2);
    let evicted = rendezvous.wait_for(1).unwrap();
    rendezvous.forget(1).unwrap();
    assert!(evicted.await.is_err(), "the evicted value is gone");
    assert_eq!(rendezvous.wait_for(3).unwrap().await, Ok(()));

    let unbuffered: Rendezvous<u8, ()> = Rendezvous::new(0);
    unbuffered.observe(1, ()).unwrap();
    assert_eq!(unbuffered.len().unwrap(), 0);
}
