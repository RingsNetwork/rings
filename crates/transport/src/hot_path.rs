//! Operation counters of the hot paths (load-bounds paper, Obligation "Admission complexity",
//! #938), for the deterministic simulation only: the module exists with the `dummy` feature
//! alone, so a production build counts nothing and pays nothing.
//!
//! Each hot path counts its events and the operations they cost, per thread, so a
//! current-thread simulation reads its own counts while other tests run in parallel:
//!
//! ```text
//!   path               event                    operation
//!   frame admission    a frame admitted         a link or pool lock taken to admit it
//!   frame release      a frame released         a link or pool lock taken to release it
//!   queue service      an arrival, release, …   a waiter visited
//!   pool service       a service of the pool    a demand visited
//!   protocol sends     a send queued            a task spawned for it
//!   inbox withdrawal   a fact recorded          a queued item scanned
//! ```

use std::cell::Cell;

/// One hot path of the obligation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotPath {
    /// A received frame's admission and the locks it takes.
    FrameAdmission,
    /// A received frame's release and the locks it takes.
    FrameRelease,
    /// A `ResourceOrderedQueue` event and the waiters it visits.
    QueueService,
    /// A service of the node's frame pool and the demands it visits.
    PoolService,
    /// A queued protocol send and the tasks spawned for it.
    ProtocolSend,
    /// A fact recorded in the application inbox and the items it scans.
    InboxRecord,
}

impl HotPath {
    /// Every hot path, in table order.
    pub const ALL: [Self; 6] = [
        Self::FrameAdmission,
        Self::FrameRelease,
        Self::QueueService,
        Self::PoolService,
        Self::ProtocolSend,
        Self::InboxRecord,
    ];

    /// This path's count among `counts`, which hold one per path in table order.
    fn of(self, counts: &mut [HotPathCount; 6]) -> &mut HotPathCount {
        let [admission, release, queue, pool, send, inbox] = counts;
        match self {
            Self::FrameAdmission => admission,
            Self::FrameRelease => release,
            Self::QueueService => queue,
            Self::PoolService => pool,
            Self::ProtocolSend => send,
            Self::InboxRecord => inbox,
        }
    }
}

/// The counts of one hot path: its events, the operations they cost, and the most one event
/// cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HotPathCount {
    /// Events.
    pub events: u64,
    /// Operations, over every event.
    pub operations: u64,
    /// The most operations one event cost.
    pub max: u64,
    /// The operations of the latest event so far.
    current: u64,
}

thread_local! {
    /// This thread's counts, one per [`HotPath`].
    static COUNTS: Cell<[HotPathCount; 6]> = const { Cell::new([HotPathCount {
        events: 0,
        operations: 0,
        max: 0,
        current: 0,
    }; 6]) };
}

/// Count one event of `path` that cost `operations`.
pub fn record(path: HotPath, operations: u64) {
    COUNTS.with(|counts| {
        let mut all = counts.get();
        let count = path.of(&mut all);
        count.events = count.events.saturating_add(1);
        count.operations = count.operations.saturating_add(operations);
        count.current = operations;
        count.max = count.max.max(operations);
        counts.set(all);
    });
}

/// Charge `operations` more to the latest event of `path`: the work a service does after it
/// started, or the release of a frame admitted earlier. It counts no event; the maximum is of
/// the event's total so far.
pub fn charge(path: HotPath, operations: u64) {
    COUNTS.with(|counts| {
        let mut all = counts.get();
        let count = path.of(&mut all);
        count.operations = count.operations.saturating_add(operations);
        count.current = count.current.saturating_add(operations);
        count.max = count.max.max(count.current);
        counts.set(all);
    });
}

thread_local! {
    /// The locks this thread took on the received-frame path's state, as the next reading of
    /// [`locks`] sees them.
    static LOCKS: Cell<u64> = const { Cell::new(0) };
    /// The pool services this thread is inside, whose locks the pool service counts instead.
    static SERVICES: Cell<u64> = const { Cell::new(0) };
}

/// Count one lock taken on a link's or the pool's state, unless a pool service takes it: a
/// frame counts its own locks, and the service the frame starts counts its own work.
pub(crate) fn lock_taken() {
    if SERVICES.with(Cell::get) == 0 {
        LOCKS.with(|locks| locks.set(locks.get().saturating_add(1)));
    }
}

/// A pool service runs until the returned guard drops.
pub(crate) fn service() -> ServiceGuard {
    SERVICES.with(|services| services.set(services.get().saturating_add(1)));
    ServiceGuard
}

/// The pool service [`service`] entered, left on drop.
pub(crate) struct ServiceGuard;

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        SERVICES.with(|services| services.set(services.get().saturating_sub(1)));
    }
}

/// The locks this thread has taken on a link's or the pool's state.
pub(crate) fn locks() -> u64 {
    LOCKS.with(Cell::get)
}

/// This thread's count of `path`.
pub fn count(path: HotPath) -> HotPathCount {
    COUNTS.with(|counts| *path.of(&mut counts.get()))
}

/// Reset this thread's counts.
pub fn reset() {
    COUNTS.with(|counts| counts.set([HotPathCount::default(); 6]));
}
