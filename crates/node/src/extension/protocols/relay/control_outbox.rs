//! Bounded per-peer off-gate actors for relay terminal control frames.

use std::collections::HashMap;
#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
use std::collections::HashSet;
#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
use std::sync::atomic::AtomicBool;
#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::extension::ext::Scope;

/// Global bound over in-flight and buffered terminal sends. A peer lane is retained only while it
/// contributes at least one send to this count, so this also bounds the number of worker tasks.
const MAX_PENDING_RELAY_CONTROL_SENDS: usize = 64;
/// Per-peer share of the global control-send budget. A rejected-Open burst from one authenticated
/// peer cannot crowd terminal frames for unrelated peers out of the global outbox.
const MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER: usize = 4;

struct ControlSend {
    scope: Scope,
    to: Did,
    payload: Bytes,
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

async fn apply_control_send(control: ControlSend) {
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    if let Some(hook) = control.test_hook.as_ref() {
        hook.before_send().await;
    }
    if let Err(error) = control.scope.send(control.to, control.payload).await {
        tracing::debug!(peer = %control.to, ?error, "relay terminal control send failed");
    }
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    if let Some(hook) = control.test_hook.as_ref() {
        if let Err(error) = hook.record_completed(control.to) {
            tracing::debug!(?error, "relay control-send test hook failed");
        }
    }
}

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
pub(crate) struct ControlSendTestHook {
    block_first: AtomicBool,
    blocked: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: Mutex<HashSet<Did>>,
    completion: tokio::sync::Notify,
}

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
impl Default for ControlSendTestHook {
    fn default() -> Self {
        Self {
            block_first: AtomicBool::new(true),
            blocked: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            completed: Mutex::new(HashSet::new()),
            completion: tokio::sync::Notify::new(),
        }
    }
}

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
impl ControlSendTestHook {
    async fn before_send(&self) {
        if self.block_first.swap(false, Ordering::AcqRel) {
            self.blocked.store(true, Ordering::Release);
            self.entered.notify_one();
            self.release.notified().await;
        }
    }

    fn record_completed(&self, peer: Did) -> crate::error::Result<()> {
        lock_state(&self.completed)?.insert(peer);
        self.completion.notify_waiters();
        Ok(())
    }

    pub(crate) async fn wait_until_blocked(&self) {
        while !self.blocked.load(Ordering::Acquire) {
            let entered = self.entered.notified();
            if self.blocked.load(Ordering::Acquire) {
                return;
            }
            entered.await;
        }
    }

    pub(crate) async fn wait_until_completed(&self, peer: Did) -> crate::error::Result<()> {
        loop {
            let completion = self.completion.notified();
            if lock_state(&self.completed)?.contains(&peer) {
                return Ok(());
            }
            completion.await;
        }
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

struct ControlLaneBudget {
    generation: u64,
    pending: usize,
}

/// Pure admission/accounting state for peer workers.
///
/// Invariant: `pending = sum(by_peer[*].pending) <= MAX_PENDING_RELAY_CONTROL_SENDS` and every
/// retained lane has `1..=MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER` sends after `enqueue` returns.
/// A newly opened lane may transiently have zero pending sends while the outbox mutex is held.
/// Preservation: reservation increments the lane and global counters atomically; completion
/// decrements both; abort removes exactly one generation and its entire contribution.
#[derive(Default)]
struct ControlBudget {
    by_peer: HashMap<Did, ControlLaneBudget>,
    pending: usize,
    next_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LaneCompletion {
    Retained,
    Retired,
    Stale,
}

impl ControlBudget {
    fn ensure_capacity(&self, peer: Did) -> crate::error::Result<()> {
        if self.pending >= MAX_PENDING_RELAY_CONTROL_SENDS {
            return Err(capacity_error(peer, "global control-send budget is full"));
        }
        if self
            .by_peer
            .get(&peer)
            .is_some_and(|lane| lane.pending >= MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER)
        {
            return Err(capacity_error(peer, "per-peer control-send budget is full"));
        }
        Ok(())
    }

    fn contains_lane(&self, peer: Did) -> bool {
        self.by_peer.contains_key(&peer)
    }

    fn open_lane(&mut self, peer: Did) -> crate::error::Result<u64> {
        if self.contains_lane(peer) {
            return Err(capacity_error(peer, "control lane already exists"));
        }
        self.ensure_capacity(peer)?;
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .ok_or_else(|| capacity_error(peer, "control-lane generation space is exhausted"))?;
        self.by_peer.insert(peer, ControlLaneBudget {
            generation,
            pending: 0,
        });
        Ok(generation)
    }

    fn reserve(&mut self, peer: Did) -> crate::error::Result<u64> {
        self.ensure_capacity(peer)?;
        let lane = self.by_peer.get_mut(&peer).ok_or_else(|| {
            crate::error::Error::ExtensionError(format!(
                "relay control lane for {peer} disappeared before reservation"
            ))
        })?;
        let lane_pending = lane
            .pending
            .checked_add(1)
            .ok_or_else(|| capacity_error(peer, "per-peer control-send counter overflowed"))?;
        let pending = self
            .pending
            .checked_add(1)
            .ok_or_else(|| capacity_error(peer, "global control-send counter overflowed"))?;
        lane.pending = lane_pending;
        self.pending = pending;
        Ok(lane.generation)
    }

    fn complete(&mut self, peer: Did, generation: u64) -> crate::error::Result<LaneCompletion> {
        let Some(lane) = self.by_peer.get_mut(&peer) else {
            return Ok(LaneCompletion::Stale);
        };
        if lane.generation != generation {
            return Ok(LaneCompletion::Stale);
        }
        let lane_pending = lane.pending.checked_sub(1).ok_or_else(|| {
            capacity_error(peer, "live control lane has no pending send to complete")
        })?;
        let pending = self
            .pending
            .checked_sub(1)
            .ok_or_else(|| capacity_error(peer, "global control-send count underflowed"))?;
        lane.pending = lane_pending;
        self.pending = pending;
        let retire = lane.pending == 0;
        if retire {
            self.by_peer.remove(&peer);
            Ok(LaneCompletion::Retired)
        } else {
            Ok(LaneCompletion::Retained)
        }
    }

    fn abort_generation(&mut self, peer: Did, generation: u64) -> crate::error::Result<bool> {
        let Some(lane) = self.by_peer.get(&peer) else {
            return Ok(false);
        };
        if lane.generation != generation {
            return Ok(false);
        }
        let pending = self.pending.checked_sub(lane.pending).ok_or_else(|| {
            capacity_error(peer, "aborted lane exceeds global control-send count")
        })?;
        self.by_peer.remove(&peer);
        self.pending = pending;
        Ok(true)
    }
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
type PlatformSender = tokio::sync::mpsc::Sender<ControlSend>;

#[cfg(all(feature = "browser", target_family = "wasm"))]
type PlatformSender = futures::channel::mpsc::Sender<ControlSend>;

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
type PlatformReceiver = tokio::sync::mpsc::Receiver<ControlSend>;

#[cfg(all(feature = "browser", target_family = "wasm"))]
type PlatformReceiver = futures::channel::mpsc::Receiver<ControlSend>;

/// Effect-state invariant while the outbox mutex is not held: `senders.keys()` equals
/// `budget.by_peer.keys()`. Opening publishes both entries in one critical section; retirement
/// and generation abort remove both entries in the same critical section.
struct ControlOutboxState<S> {
    budget: ControlBudget,
    senders: HashMap<Did, S>,
}

impl<S> Default for ControlOutboxState<S> {
    fn default() -> Self {
        Self {
            budget: ControlBudget::default(),
            senders: HashMap::new(),
        }
    }
}

impl<S> ControlOutboxState<S> {
    fn open_lane(&mut self, peer: Did, sender: S) -> crate::error::Result<u64> {
        if self.senders.contains_key(&peer) {
            return Err(capacity_error(peer, "orphaned control-lane sender exists"));
        }
        let generation = self.budget.open_lane(peer)?;
        self.senders.insert(peer, sender);
        Ok(generation)
    }

    fn complete(&mut self, peer: Did, generation: u64) -> crate::error::Result<LaneCompletion> {
        let completion = self.budget.complete(peer, generation)?;
        if completion == LaneCompletion::Retired {
            self.senders.remove(&peer);
        }
        Ok(completion)
    }

    fn abort_generation(&mut self, peer: Did, generation: u64) -> crate::error::Result<bool> {
        let aborted = self.budget.abort_generation(peer, generation)?;
        if aborted {
            self.senders.remove(&peer);
        }
        Ok(aborted)
    }
}

type PlatformControlState = ControlOutboxState<PlatformSender>;

#[derive(Default)]
pub(super) struct ControlOutbox {
    state: Arc<Mutex<PlatformControlState>>,
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

impl ControlOutbox {
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    pub(super) fn with_test_hook(hook: Arc<ControlSendTestHook>) -> Self {
        Self {
            state: Arc::new(Mutex::new(PlatformControlState::default())),
            test_hook: Some(hook),
        }
    }

    pub(super) fn enqueue(
        &self,
        scope: Scope,
        to: Did,
        payload: Bytes,
    ) -> crate::error::Result<()> {
        let mut state = lock_state(self.state.as_ref())?;
        state.budget.ensure_capacity(to)?;
        let mut new_receiver = None;
        if !state.budget.contains_lane(to) {
            let (sender, receiver) = platform_channel();
            let generation = state.open_lane(to, sender)?;
            new_receiver = Some((generation, receiver));
        }
        let generation = match state.budget.reserve(to) {
            Ok(generation) => generation,
            Err(error) => {
                if let Some((generation, _)) = new_receiver.as_ref() {
                    state.abort_generation(to, *generation)?;
                }
                return Err(error);
            }
        };
        let send = ControlSend {
            scope,
            to,
            payload,
            #[cfg(all(
                test,
                feature = "node",
                not(all(feature = "browser", target_family = "wasm"))
            ))]
            test_hook: self.test_hook.clone(),
        };
        let result = match state.senders.get_mut(&to) {
            Some(sender) => platform_try_send(sender, send),
            None => {
                state.abort_generation(to, generation)?;
                return Err(capacity_error(
                    to,
                    "control-lane sender disappeared before enqueue",
                ));
            }
        };
        if let Err(error) = result {
            match error.kind {
                PlatformSendFailureKind::Full => {
                    state.complete(to, generation)?;
                }
                PlatformSendFailureKind::Closed => {
                    state.abort_generation(to, generation)?;
                }
            }
            return Err(capacity_error(to, error.reason.as_str()));
        }
        if let Some((generation, receiver)) = new_receiver {
            spawn_platform_lane(Arc::clone(&self.state), to, generation, receiver);
        }
        Ok(())
    }
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
fn platform_channel() -> (PlatformSender, PlatformReceiver) {
    tokio::sync::mpsc::channel(MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER)
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn platform_channel() -> (PlatformSender, PlatformReceiver) {
    futures::channel::mpsc::channel(MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlatformSendFailureKind {
    Full,
    Closed,
}

struct PlatformSendFailure {
    kind: PlatformSendFailureKind,
    reason: String,
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
fn platform_try_send(
    sender: &mut PlatformSender,
    send: ControlSend,
) -> Result<(), PlatformSendFailure> {
    sender.try_send(send).map_err(|error| {
        let kind = match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => PlatformSendFailureKind::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => PlatformSendFailureKind::Closed,
        };
        PlatformSendFailure {
            kind,
            reason: error.to_string(),
        }
    })
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn platform_try_send(
    sender: &mut PlatformSender,
    send: ControlSend,
) -> Result<(), PlatformSendFailure> {
    sender.try_send(send).map_err(|error| {
        let kind = if error.is_full() {
            PlatformSendFailureKind::Full
        } else {
            PlatformSendFailureKind::Closed
        };
        PlatformSendFailure {
            kind,
            reason: error.to_string(),
        }
    })
}

struct LaneLifecycle<S> {
    state: Arc<Mutex<ControlOutboxState<S>>>,
    peer: Did,
    generation: u64,
}

impl<S> LaneLifecycle<S> {
    fn new(state: Arc<Mutex<ControlOutboxState<S>>>, peer: Did, generation: u64) -> Self {
        Self {
            state,
            peer,
            generation,
        }
    }

    fn complete_send(&self) -> crate::error::Result<LaneCompletion> {
        complete_send(self.state.as_ref(), self.peer, self.generation)
    }
}

impl<S> Drop for LaneLifecycle<S> {
    /// Cancellation is the exceptional exit of the worker algebra. Reclaiming the whole matching
    /// generation drops its sender, closes the receiver, and restores the global budget. A stale
    /// worker cannot remove a replacement because generation equality is required.
    fn drop(&mut self) {
        if let Err(error) = abort_lane(self.state.as_ref(), self.peer, self.generation) {
            tracing::debug!(peer = %self.peer, ?error, "relay control lane cleanup failed");
        }
    }
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
fn spawn_platform_lane(
    state: Arc<Mutex<PlatformControlState>>,
    peer: Did,
    generation: u64,
    receiver: PlatformReceiver,
) {
    tokio::spawn(run_platform_lane(state, peer, generation, receiver));
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
fn spawn_platform_lane(
    state: Arc<Mutex<PlatformControlState>>,
    peer: Did,
    generation: u64,
    receiver: PlatformReceiver,
) {
    wasm_bindgen_futures::spawn_local(run_platform_lane(state, peer, generation, receiver));
}

async fn run_platform_lane(
    state: Arc<Mutex<PlatformControlState>>,
    peer: Did,
    generation: u64,
    mut receiver: PlatformReceiver,
) {
    let lifecycle = LaneLifecycle::new(state, peer, generation);
    while let Some(control) = receive_platform_control(&mut receiver).await {
        apply_control_send(control).await;
        match lifecycle.complete_send() {
            Ok(LaneCompletion::Retained) => {}
            Ok(LaneCompletion::Retired | LaneCompletion::Stale) => break,
            Err(error) => {
                tracing::debug!(%peer, ?error, "relay control lane could not retire send");
                break;
            }
        }
    }
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
async fn receive_platform_control(receiver: &mut PlatformReceiver) -> Option<ControlSend> {
    receiver.recv().await
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
async fn receive_platform_control(receiver: &mut PlatformReceiver) -> Option<ControlSend> {
    use futures::StreamExt as _;

    receiver.next().await
}

fn complete_send<S>(
    state: &Mutex<ControlOutboxState<S>>,
    peer: Did,
    generation: u64,
) -> crate::error::Result<LaneCompletion> {
    lock_state(state)?.complete(peer, generation)
}

fn abort_lane<S>(
    state: &Mutex<ControlOutboxState<S>>,
    peer: Did,
    generation: u64,
) -> crate::error::Result<bool> {
    lock_state(state)?.abort_generation(peer, generation)
}

fn lock_state<T>(slot: &Mutex<T>) -> crate::error::Result<std::sync::MutexGuard<'_, T>> {
    slot.lock().map_err(|_| {
        crate::error::Error::ExtensionError("relay control outbox lock is poisoned".to_string())
    })
}

fn capacity_error(peer: Did, reason: &str) -> crate::error::Error {
    crate::error::Error::ExtensionError(format!(
        "relay terminal control outbox rejected frame for {peer}: {reason}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_peer_budget_cannot_consume_another_peers_share() -> crate::error::Result<()> {
        let mut budget = ControlBudget::default();
        let busy_peer = Did::from(1_u32);
        let other_peer = Did::from(2_u32);
        let busy_generation = budget.open_lane(busy_peer)?;

        for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
            assert_eq!(budget.reserve(busy_peer)?, busy_generation);
        }
        assert!(budget.ensure_capacity(busy_peer).is_err());

        let other_generation = budget.open_lane(other_peer)?;
        assert_eq!(budget.reserve(other_peer)?, other_generation);
        assert_eq!(
            budget.complete(other_peer, other_generation)?,
            LaneCompletion::Retired
        );
        Ok(())
    }

    #[test]
    fn global_budget_rejects_at_the_exact_bound_and_recovers() -> crate::error::Result<()> {
        let mut budget = ControlBudget::default();
        let first_peer = Did::from(10_u32);
        let first_generation = budget.open_lane(first_peer)?;
        for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
            budget.reserve(first_peer)?;
        }
        for peer_id in 11_u32..26_u32 {
            let peer = Did::from(peer_id);
            budget.open_lane(peer)?;
            for _ in 0..MAX_PENDING_RELAY_CONTROL_SENDS_PER_PEER {
                budget.reserve(peer)?;
            }
        }

        assert_eq!(budget.pending, MAX_PENDING_RELAY_CONTROL_SENDS);
        let recovering_peer = Did::from(100_u32);
        assert!(budget.open_lane(recovering_peer).is_err());
        assert_eq!(
            budget.complete(first_peer, first_generation)?,
            LaneCompletion::Retained
        );
        let recovering_generation = budget.open_lane(recovering_peer)?;
        assert_eq!(budget.reserve(recovering_peer)?, recovering_generation);
        assert_eq!(budget.pending, MAX_PENDING_RELAY_CONTROL_SENDS);
        Ok(())
    }

    #[test]
    fn generation_exhaustion_does_not_publish_a_lane() {
        let mut budget = ControlBudget {
            next_generation: u64::MAX,
            ..ControlBudget::default()
        };
        let peer = Did::from(101_u32);

        assert!(budget.open_lane(peer).is_err());
        assert!(!budget.contains_lane(peer));
        assert_eq!(budget.pending, 0);
    }

    #[test]
    fn abort_reclaims_capacity_and_stale_completion_preserves_replacement(
    ) -> crate::error::Result<()> {
        let mut state = ControlOutboxState::default();
        let peer = Did::from(3_u32);
        let old_generation = state.open_lane(peer, 7_u8)?;
        state.budget.reserve(peer)?;
        state.budget.reserve(peer)?;

        assert!(state.abort_generation(peer, old_generation)?);
        assert_eq!(state.budget.pending, 0);
        assert!(!state.budget.contains_lane(peer));
        assert!(!state.senders.contains_key(&peer));

        let replacement_generation = state.open_lane(peer, 9_u8)?;
        assert_ne!(replacement_generation, old_generation);
        state.budget.reserve(peer)?;
        assert_eq!(state.complete(peer, old_generation)?, LaneCompletion::Stale);
        assert_eq!(state.budget.pending, 1);
        assert!(state.budget.contains_lane(peer));
        assert_eq!(state.senders.get(&peer), Some(&9_u8));
        assert_eq!(
            state.complete(peer, replacement_generation)?,
            LaneCompletion::Retired
        );
        assert_eq!(state.budget.pending, 0);
        assert!(!state.senders.contains_key(&peer));
        Ok(())
    }

    #[test]
    fn lane_lifecycle_drop_reclaims_the_entire_generation() -> crate::error::Result<()> {
        let peer = Did::from(4_u32);
        let state = Arc::new(Mutex::new(ControlOutboxState::default()));
        let generation = {
            let mut locked = lock_state(state.as_ref())?;
            let generation = locked.open_lane(peer, 11_u8)?;
            locked.budget.reserve(peer)?;
            locked.budget.reserve(peer)?;
            generation
        };

        {
            let _lifecycle = LaneLifecycle::new(Arc::clone(&state), peer, generation);
        }
        let locked = lock_state(state.as_ref())?;
        assert_eq!(locked.budget.pending, 0);
        assert!(!locked.budget.contains_lane(peer));
        assert!(!locked.senders.contains_key(&peer));
        Ok(())
    }
}
