#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
use std::collections::BTreeMap;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use super::Did;
use super::TransferClass;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::PerSlot;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::SessionControl;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::SessionRef;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
thread_local! {
    static OUTBOUND_SUBMIT_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Session questions this thread's nodes answered with an announcement.
    static SESSION_ANNOUNCEMENT_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Payload frames this thread's nodes encoded with at least one session by reference.
    static REFERENCED_FRAME_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn reset_outbound_submit_count_for_test() {
    OUTBOUND_SUBMIT_COUNT.with(|count| count.set(0));
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn outbound_submit_count_for_test() -> usize {
    OUTBOUND_SUBMIT_COUNT.with(Cell::get)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_outbound_submit() {
    OUTBOUND_SUBMIT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
}

/// The session questions answered with an announcement on this test thread so far: the
/// observable that a miss was repaired by the link, not avoided.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn session_announcement_count_for_test() -> usize {
    SESSION_ANNOUNCEMENT_COUNT.with(Cell::get)
}

/// The payload frames encoded with at least one session slot by reference on this test thread
/// so far: the observable that a link switched from inline to references.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn referenced_frame_count_for_test() -> usize {
    REFERENCED_FRAME_COUNT.with(Cell::get)
}

/// Count a frame encoded as `sessions` on this test thread if any slot is a reference.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_encoded_frame(sessions: &PerSlot<SessionRef<'_>>) {
    let referenced = matches!(sessions.origin, SessionRef::Digest(_))
        || matches!(sessions.hop, SessionRef::Digest(_));
    if referenced {
        REFERENCED_FRAME_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    }
}

/// Count `answer` on this test thread if it is an announcement.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_session_answer(answer: &SessionControl) {
    if matches!(answer, SessionControl::Announce(_)) {
        SESSION_ANNOUNCEMENT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    }
}

type FrameAdmission = (TransferClass, u64, usize);

static OUTBOUND_FRAME_TRACES: Mutex<BTreeMap<Did, Vec<FrameAdmission>>> =
    Mutex::new(BTreeMap::new());
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
static PAUSED_WORKERS: Mutex<BTreeSet<Did>> = Mutex::new(BTreeSet::new());
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
static ACTIVE_TRANSFERS: Mutex<BTreeMap<Did, usize>> = Mutex::new(BTreeMap::new());
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
static SUBMITTED_TRANSFERS: Mutex<BTreeMap<Did, usize>> = Mutex::new(BTreeMap::new());
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
static HANDLED_TRANSFERS: Mutex<BTreeMap<Did, usize>> = Mutex::new(BTreeMap::new());
static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(0);
const WORKER_ID_STRIDE: u64 = 1 << 32;

pub(super) fn worker_transfer_id_base() -> u64 {
    NEXT_WORKER_ID
        .fetch_add(1, Ordering::Relaxed)
        .saturating_mul(WORKER_ID_STRIDE)
}

fn lock_traces() -> std::sync::MutexGuard<'static, BTreeMap<Did, Vec<FrameAdmission>>> {
    OUTBOUND_FRAME_TRACES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn lock_paused_workers() -> std::sync::MutexGuard<'static, BTreeSet<Did>> {
    PAUSED_WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn lock_active_transfers() -> std::sync::MutexGuard<'static, BTreeMap<Did, usize>> {
    ACTIVE_TRANSFERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) struct ActiveTransferGuard {
    peer: Did,
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl ActiveTransferGuard {
    pub(super) fn enter(peer: Did) -> Self {
        let mut active = lock_active_transfers();
        let count = active.entry(peer).or_default();
        *count = count.saturating_add(1);
        Self { peer }
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl Drop for ActiveTransferGuard {
    fn drop(&mut self) {
        let mut active = lock_active_transfers();
        if let Some(count) = active.get_mut(&self.peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                active.remove(&self.peer);
            }
        }
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn lock_counts(
    counts: &'static Mutex<BTreeMap<Did, usize>>,
) -> std::sync::MutexGuard<'static, BTreeMap<Did, usize>> {
    counts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn pause_worker(peer: Did) {
    lock_counts(&SUBMITTED_TRANSFERS).insert(peer, 0);
    lock_counts(&HANDLED_TRANSFERS).insert(peer, 0);
    lock_paused_workers().insert(peer);
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn resume_worker(peer: Did) {
    lock_paused_workers().remove(&peer);
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn worker_is_paused(peer: Did) -> bool {
    lock_paused_workers().contains(&peer)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_submission(peer: Did) {
    let mut submitted = lock_counts(&SUBMITTED_TRANSFERS);
    let count = submitted.entry(peer).or_default();
    *count = count.saturating_add(1);
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_handled_submission(peer: Did) {
    let mut handled = lock_counts(&HANDLED_TRANSFERS);
    let count = handled.entry(peer).or_default();
    *count = count.saturating_add(1);
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn buffered_submissions(peer: Did) -> usize {
    let submitted = lock_counts(&SUBMITTED_TRANSFERS)
        .get(&peer)
        .copied()
        .unwrap_or_default();
    let handled = lock_counts(&HANDLED_TRANSFERS)
        .get(&peer)
        .copied()
        .unwrap_or_default();
    submitted.saturating_sub(handled)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn submitted_transfers(peer: Did) -> usize {
    lock_counts(&SUBMITTED_TRANSFERS)
        .get(&peer)
        .copied()
        .unwrap_or_default()
}

pub(super) fn record(peer: Did, class: TransferClass, transfer_id: u64) {
    if let Some(trace) = lock_traces().get_mut(&peer) {
        let frame_ordinal = trace
            .iter()
            .filter(|(_, observed_id, _)| *observed_id == transfer_id)
            .count();
        trace.push((class, transfer_id, frame_ordinal));
    }
}

impl super::super::SwarmTransport {
    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn pause_outbound_worker_for_test(&self, peer: Did) {
        pause_worker(peer);
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn resume_outbound_worker_for_test(&self, peer: Did) {
        resume_worker(peer);
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn outbound_buffered_submissions_for_test(&self, peer: Did) -> usize {
        buffered_submissions(peer)
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn outbound_submitted_transfers_for_test(&self, peer: Did) -> usize {
        submitted_transfers(peer)
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn outbound_worker_has_active_transfer_for_test(&self, peer: Did) -> bool {
        lock_active_transfers().contains_key(&peer)
    }

    pub(crate) fn start_outbound_frame_trace_for_test(&self, peer: Did) {
        lock_traces().insert(peer, Vec::new());
    }

    pub(crate) fn take_outbound_frame_trace_for_test(&self, peer: Did) -> Vec<FrameAdmission> {
        lock_traces().remove(&peer).unwrap_or_default()
    }

    pub(crate) fn outbound_frame_trace_for_test(&self, peer: Did) -> Vec<FrameAdmission> {
        lock_traces().get(&peer).cloned().unwrap_or_default()
    }
}

#[test]
fn test_replacement_workers_receive_disjoint_transfer_id_ranges() {
    let first = worker_transfer_id_base();
    let second = worker_transfer_id_base();

    assert_ne!(first, second);
    assert!(first.abs_diff(second) >= WORKER_ID_STRIDE);
}

#[cfg(not(target_family = "wasm"))]
impl crate::swarm::transport::SwarmTransport {
    pub(crate) fn outbound_admitted_transfer_total_for_test(&self) -> usize {
        self.outbound_schedulers.admitted_transfer_total_for_test()
    }
}
