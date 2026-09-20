#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::cell::Cell;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::cell::RefCell;
use std::collections::BTreeMap;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::collections::BTreeSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use super::Did;
use super::TransferClass;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::LinkControl;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::PerSlot;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::SessionRef;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::session::SessionDigest;

/// One direction of one link, as the sending end names it: `(this node, next hop)`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) type LinkDirection = (Did, Did);

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
thread_local! {
    static OUTBOUND_SUBMIT_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Session questions this thread's nodes answered, with an announcement or a disclaimer.
    static SESSION_ANSWER_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Per link direction, the digests this thread's nodes put in each slot of a payload frame,
    /// one entry per frame whose slot went by reference.
    static REFERENCED_SLOTS: RefCell<BTreeMap<LinkDirection, PerSlot<Vec<SessionDigest>>>> =
        const { RefCell::new(BTreeMap::new()) };
    /// Every link-control frame this thread's nodes emitted, with the peer it went to.
    static EMITTED_LINK_CONTROL: RefCell<Vec<(Did, LinkControl)>> =
        const { RefCell::new(Vec::new()) };
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

/// The session questions answered on this test thread so far, announcements and disclaimers
/// alike: the observable that the miss path ran, whichever way it ended.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn session_answer_count_for_test() -> usize {
    SESSION_ANSWER_COUNT.with(Cell::get)
}

/// Per link direction, how many payload frames went with the origin slot and with the hop slot
/// by reference: the observable that a link switched from inline to references, slot by slot.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn referenced_slots_for_test(link: LinkDirection) -> PerSlot<usize> {
    REFERENCED_SLOTS.with(|slots| {
        slots
            .borrow()
            .get(&link)
            .map_or(PerSlot { origin: 0, hop: 0 }, |digests| PerSlot {
                origin: digests.origin.len(),
                hop: digests.hop.len(),
            })
    })
}

/// The sessions that went by reference in the origin slot on `link`: the observable that a
/// forwarding hop referenced a session that is not its own.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn referenced_origins_for_test(link: LinkDirection) -> BTreeSet<SessionDigest> {
    REFERENCED_SLOTS.with(|slots| {
        slots
            .borrow()
            .get(&link)
            .map(|digests| digests.origin.iter().copied().collect())
            .unwrap_or_default()
    })
}

/// Per link direction, the referenced-slot counts of [`referenced_slots_for_test`], for every
/// link this test thread has seen. The counts only grow, so a scenario that reads them before
/// and after itself learns on which links references happened during it, whatever earlier
/// scenarios on the same thread, with the same deterministic node keys, did.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn referenced_links_for_test() -> BTreeMap<LinkDirection, PerSlot<usize>> {
    REFERENCED_SLOTS.with(|slots| {
        slots
            .borrow()
            .iter()
            .map(|(link, digests)| {
                (*link, PerSlot {
                    origin: digests.origin.len(),
                    hop: digests.hop.len(),
                })
            })
            .collect()
    })
}

/// Every link-control frame emitted on this test thread so far, in emission order, with the
/// peer it was addressed to.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn emitted_link_control_for_test() -> Vec<(Did, LinkControl)> {
    EMITTED_LINK_CONTROL.with(|emitted| emitted.borrow().clone())
}

/// The digest a slot was sent as, if it went by reference.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn referenced_digest(slot: &SessionRef<'_>) -> Option<SessionDigest> {
    match slot {
        SessionRef::Digest(digest) => Some(*digest),
        SessionRef::Inline(_) => None,
    }
}

/// Count the referenced slots of a frame encoded as `sessions` on `link`.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_encoded_frame(link: LinkDirection, sessions: &PerSlot<SessionRef<'_>>) {
    let referenced = PerSlot {
        origin: referenced_digest(&sessions.origin),
        hop: referenced_digest(&sessions.hop),
    };
    if referenced.origin.is_none() && referenced.hop.is_none() {
        return;
    }
    REFERENCED_SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let digests = slots.entry(link).or_insert(PerSlot {
            origin: Vec::new(),
            hop: Vec::new(),
        });
        digests.origin.extend(referenced.origin);
        digests.hop.extend(referenced.hop);
    });
}

/// Count one answered session question on this test thread.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(super) fn record_session_answer() {
    SESSION_ANSWER_COUNT.with(|count| count.set(count.get().saturating_add(1)));
}

/// Record `control` as emitted to `peer` on this test thread.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn record_emitted_link_control(peer: Did, control: &LinkControl) {
    EMITTED_LINK_CONTROL.with(|emitted| emitted.borrow_mut().push((peer, control.clone())));
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

/// The default test build has no `simulation_pressure` module, so this accessor lives with the
/// other test-only observables that every native test build compiles.
#[cfg(not(target_family = "wasm"))]
impl crate::swarm::transport::SwarmTransport {
    pub(crate) fn outbound_admitted_transfer_total_for_test(&self) -> usize {
        self.outbound_schedulers.admitted_transfer_total_for_test()
    }
}
