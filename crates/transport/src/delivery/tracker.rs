//! Imperative shell around [`DeliveryRegistry`]: one tracker per data channel.
//!
//! The tracker owns the channel's enqueued-byte counter `E` and its registry.
//! Backends supply the two channel effects through [`BufferedChannel`] and run
//! each [`RoundLease`] on their executor; they call [`DeliveryTracker::notify`]
//! on `bufferedamountlow` and [`DeliveryTracker::close`] on `close` or `error`.
//!
//! ```text
//!   send ─ enqueue ─▶ track(e) ─┐                 ┌─▶ DeliveryWait ─▶ Ok | Err
//!   bufferedamountlow ─ notify ─┼─▶ RoundLease ─▶ │   (reads its slot)
//!   close | error ─── close ────┴─────────────────┘
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use super::closed_before_flush;
use super::registry::DeliveryRegistry;
use super::registry::Step;
use super::registry::Ticket;
use super::registry::Verdict;
use crate::error::Result;
use crate::sync_utils::lock_recover;

/// The two effects a settle round performs on a data channel.
///
/// Both are observations or writes of the channel's own buffer state; neither
/// may wait for the buffer to drain.
pub(crate) trait BufferedChannel {
    /// Set the channel's `bufferedAmountLowThreshold` to `threshold` bytes.
    async fn arm_low_threshold(&self, threshold: u64);

    /// Read the channel's current `bufferedAmount` in bytes.
    async fn observe_buffered(&self) -> u64;
}

/// Delivery bookkeeping of one data channel: the counter `E` and the registry.
#[derive(Debug, Default)]
pub(crate) struct DeliveryTracker {
    /// Total bytes ever accepted into this channel's send queue (`E`).
    enqueued: Arc<AtomicU64>,
    /// Pending sends and the settle-round phase.
    registry: Mutex<DeliveryRegistry>,
}

impl DeliveryTracker {
    /// The channel's enqueued-byte counter `E`, advanced by the send path.
    pub(crate) fn enqueued(&self) -> &Arc<AtomicU64> {
        &self.enqueued
    }

    /// Lock the registry, recovering it from a poisoned lock.
    fn registry(&self) -> MutexGuard<'_, DeliveryRegistry> {
        lock_recover(&self.registry)
    }

    /// Register a send ending at `end_offset`, right after its enqueue.
    ///
    /// Returns its delivery future and, when no round runs, the lease the
    /// caller must run: the registration re-arms `τ` against the advanced `E`.
    pub(crate) fn track(self: &Arc<Self>, end_offset: u64) -> (DeliveryWait, Option<RoundLease>) {
        let (ticket, start) = {
            let mut registry = self.registry();
            let ticket = registry.register(end_offset);
            (ticket, registry.request_round())
        };
        let wait = DeliveryWait {
            tracker: Arc::clone(self),
            ticket,
        };
        (wait, start.then(|| RoundLease::new(Arc::clone(self))))
    }

    /// Handle one `bufferedamountlow` event: the lease to run, if no round runs.
    pub(crate) fn notify(self: &Arc<Self>) -> Option<RoundLease> {
        let start = self.registry().request_round();
        start.then(|| RoundLease::new(Arc::clone(self)))
    }

    /// Handle `close` or `error`: every pending send resolves `Err`.
    pub(crate) fn close(&self) {
        let wakers = self.registry().close();
        wakers.into_iter().for_each(Waker::wake);
    }
}

/// The exclusive right to run the channel's settle round.
///
/// Issued only when the round phase moves `Idle → Running`. Dropping an
/// unfinished lease (a cancelled or shut-down executor) returns the phase to
/// `Idle`, so a later request can start a new round.
#[must_use = "a settle round that is never run leaves pending sends unobserved"]
pub(crate) struct RoundLease {
    /// The tracker whose round this lease runs.
    tracker: Arc<DeliveryTracker>,
    /// Whether the round reached `Idle` by itself.
    finished: bool,
}

impl RoundLease {
    /// Wrap a freshly granted round.
    fn new(tracker: Arc<DeliveryTracker>) -> Self {
        Self {
            tracker,
            finished: false,
        }
    }

    /// Run the round to quiescence: arm `τ(E)`, then read `b`, then settle.
    ///
    /// See the registry's module documentation for why arming precedes the
    /// read and why the loop ends only on a quiescent step.
    pub(crate) async fn run(mut self, channel: &impl BufferedChannel) {
        loop {
            let enqueued = self.tracker.enqueued.load(Ordering::SeqCst);
            let Some(threshold) = self.tracker.registry().begin_step(enqueued) else {
                break;
            };
            channel.arm_low_threshold(threshold).await;
            let buffered = channel.observe_buffered().await;
            let enqueued = self.tracker.enqueued.load(Ordering::SeqCst);
            let settlement = self.tracker.registry().settle(enqueued, buffered);
            settlement.wakers.into_iter().for_each(Waker::wake);
            if settlement.step == Step::Quiescent {
                break;
            }
        }
        self.finished = true;
    }
}

impl Drop for RoundLease {
    fn drop(&mut self) {
        if !self.finished {
            self.tracker.registry().abandon_round();
        }
    }
}

/// The delivery future of one tracked send: it reads its slot's verdict.
pub(crate) struct DeliveryWait {
    /// The channel tracker holding this send's slot.
    tracker: Arc<DeliveryTracker>,
    /// This send's slot.
    ticket: Ticket,
}

impl Future for DeliveryWait {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.tracker
            .registry()
            .poll(self.ticket, context.waker())
            .map(|verdict| match verdict {
                Verdict::Flushed => Ok(()),
                Verdict::Closed => Err(closed_before_flush()),
            })
    }
}

impl Drop for DeliveryWait {
    fn drop(&mut self) {
        self.tracker.registry().forget(self.ticket);
    }
}
