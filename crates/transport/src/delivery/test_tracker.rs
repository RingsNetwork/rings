//! Deterministic event-driven tests of the delivery tracker (#887).
//!
//! A [`FakeChannel`] models the channel's `bufferedAmount` and its edge-
//! triggered `bufferedamountlow` event exactly as webrtc-rs and browsers
//! define it: the event fires when a drain moves the buffer from above the
//! threshold to at or below it. Every step is an event; no test waits on time.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::Wake;
use std::task::Waker;

use super::delivery_flushed;
use super::tracker::BufferedChannel;
use super::tracker::DeliveryTracker;
use super::tracker::DeliveryWait;
use super::tracker::RoundLease;
use crate::error::Error;
use crate::sync_utils::lock_recover;

/// Where a [`FakeChannel`] suspends a round once, to interleave other events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    /// Never suspend.
    Never,
    /// Suspend once right after reading `bufferedAmount`, before settling.
    AfterObserve,
    /// Once, right after reading `bufferedAmount`, let a concurrent send of
    /// this many bytes land: into `b` first, then into `E`, as a real write does.
    EnqueueAfterObserve(u64),
}

/// Observable state of the modelled channel.
#[derive(Debug)]
struct FakeState {
    /// Current `bufferedAmount`.
    buffered: u64,
    /// Current `bufferedAmountLowThreshold`.
    threshold: u64,
    /// Every threshold a round armed, in order.
    armed: Vec<u64>,
    /// The pending one-shot suspension point.
    pause: Pause,
}

/// A data channel model with the standard edge-triggered low-water event.
#[derive(Debug)]
struct FakeChannel {
    /// The modelled channel state.
    state: Mutex<FakeState>,
    /// The tracker's counter `E`, which a concurrent send advances.
    enqueued: Arc<AtomicU64>,
}

impl FakeChannel {
    /// An empty channel whose threshold starts at zero, as specified.
    fn new(enqueued: Arc<AtomicU64>) -> Self {
        Self {
            state: Mutex::new(FakeState {
                buffered: 0,
                threshold: 0,
                armed: Vec::new(),
                pause: Pause::Never,
            }),
            enqueued,
        }
    }

    /// Enqueue `bytes`: the write counts them in `b`, then the send path stores `E`.
    fn enqueue(&self, bytes: u64) -> u64 {
        self.state().buffered += bytes;
        self.enqueued.fetch_add(bytes, Ordering::SeqCst) + bytes
    }

    /// Lock the modelled state.
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        lock_recover(&self.state)
    }
}

/// A future that is pending exactly once, then ready.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl BufferedChannel for FakeChannel {
    async fn arm_low_threshold(&self, threshold: u64) {
        let mut state = self.state();
        state.threshold = threshold;
        state.armed.push(threshold);
    }

    async fn observe_buffered(&self) -> u64 {
        let (buffered, pause) = {
            let mut state = self.state();
            let pause = std::mem::replace(&mut state.pause, Pause::Never);
            (state.buffered, pause)
        };
        match pause {
            Pause::Never => {}
            Pause::AfterObserve => YieldOnce(false).await,
            Pause::EnqueueAfterObserve(bytes) => {
                self.enqueue(bytes);
            }
        }
        buffered
    }
}

/// Waker that counts its wake-ups.
#[derive(Debug, Default)]
struct CountingWaker(AtomicUsize);

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// One channel, its tracker, and the sends issued on it.
struct Harness {
    /// The modelled channel.
    channel: FakeChannel,
    /// The tracker under test.
    tracker: Arc<DeliveryTracker>,
}

impl Harness {
    /// A fresh channel with no traffic.
    fn new() -> Self {
        let tracker = Arc::new(DeliveryTracker::default());
        Self {
            channel: FakeChannel::new(Arc::clone(tracker.enqueued())),
            tracker,
        }
    }

    /// Current `(E, b)`.
    fn observation(&self) -> (u64, u64) {
        (
            self.tracker.enqueued().load(Ordering::SeqCst),
            self.channel.state().buffered,
        )
    }

    /// Run a granted round to completion; the fake never suspends for real.
    fn run(&self, lease: Option<RoundLease>) {
        if let Some(lease) = lease {
            let mut round = pin!(lease.run(&self.channel));
            let mut context = Context::from_waker(Waker::noop());
            while round.as_mut().poll(&mut context).is_pending() {}
        }
    }

    /// Enqueue `bytes` as one send, as the backend does: advance `E` and `b`,
    /// then register the send and run the registration's round.
    fn send(&self, bytes: u64) -> (u64, DeliveryWait) {
        let end_offset = self.enqueue(bytes);
        let (wait, lease) = self.tracker.track(end_offset);
        self.run(lease);
        (end_offset, wait)
    }

    /// Advance `E` and `b` by `bytes` without registering: the queue accepted it.
    fn enqueue(&self, bytes: u64) -> u64 {
        self.channel.enqueue(bytes)
    }

    /// Drain up to `bytes` from the buffer and deliver the low-water event if
    /// the drain crossed the threshold. Returns whether the event fired.
    fn drain(&self, bytes: u64) -> bool {
        let fired = {
            let mut state = self.channel.state();
            let from = state.buffered;
            state.buffered = from.saturating_sub(bytes);
            from > state.threshold && state.buffered <= state.threshold
        };
        if fired {
            self.run(self.tracker.notify());
        }
        fired
    }
}

/// Poll a delivery future once with `waker`.
fn poll_wait(wait: &mut DeliveryWait, waker: &Waker) -> Poll<Result<(), Error>> {
    pin!(wait).poll(&mut Context::from_waker(waker))
}

/// Poll with a no-op waker.
fn poll_now(wait: &mut DeliveryWait) -> Poll<Result<(), Error>> {
    poll_wait(wait, Waker::noop())
}

/// One operation of the exhaustive schedule.
#[derive(Clone, Copy, Debug)]
enum Operation {
    /// Enqueue and register a send of this many bytes.
    Send(u64),
    /// Drain this many bytes, delivering the event on a crossing.
    Drain(u64),
}

/// All schedules of `length` operations over sizes `1..=3`.
fn schedules(length: usize) -> Vec<Vec<Operation>> {
    let alphabet: Vec<Operation> = (1..=3)
        .flat_map(|size| [Operation::Send(size), Operation::Drain(size)])
        .collect();
    (0..length).fold(vec![Vec::new()], |prefixes, _| {
        prefixes
            .into_iter()
            .flat_map(|prefix| {
                alphabet.iter().map(move |operation| {
                    let mut schedule = prefix.clone();
                    schedule.push(*operation);
                    schedule
                })
            })
            .collect()
    })
}

/// Soundness and one-event liveness over every schedule of six operations:
/// after each step, a pending send reports `Ok` iff `φ(E, b, e)` holds.
///
/// Induction on the schedule: the base (no sends) is vacuous; each `Send`
/// runs a registration round and each crossing `Drain` runs an event round,
/// so the claim at step `n + 1` only needs the rounds of step `n + 1`.
#[test]
fn test_every_schedule_reports_exactly_the_flushed_sends_within_one_event() {
    for schedule in schedules(6) {
        let harness = Harness::new();
        let mut waits: Vec<(u64, DeliveryWait, bool)> = Vec::new();
        for operation in schedule.iter().copied() {
            match operation {
                Operation::Send(bytes) => {
                    let (end_offset, wait) = harness.send(bytes);
                    waits.push((end_offset, wait, false));
                }
                Operation::Drain(bytes) => {
                    harness.drain(bytes);
                }
            }
            let (enqueued, buffered) = harness.observation();
            for (end_offset, wait, reported) in waits.iter_mut() {
                if *reported {
                    continue;
                }
                let flushed = delivery_flushed(enqueued, buffered, *end_offset);
                match poll_now(wait) {
                    Poll::Ready(Ok(())) => {
                        assert!(flushed, "unsound Ok in {schedule:?}");
                        *reported = true;
                    }
                    Poll::Ready(Err(error)) => panic!("unexpected {error} in {schedule:?}"),
                    Poll::Pending => assert!(!flushed, "lost wake-up in {schedule:?}"),
                }
            }
        }
    }
}

/// Several sends pending on one channel: the threshold always tracks the
/// earliest pending offset, and each send resolves on the event that flushes it.
#[test]
fn test_concurrent_pending_sends_share_one_threshold() {
    let harness = Harness::new();
    let wakers: Vec<Arc<CountingWaker>> = (0..4).map(|_| Arc::default()).collect();
    let mut waits: Vec<DeliveryWait> = (0..4).map(|_| harness.send(10).1).collect();
    // Each waiter polls with its own counting waker, as one task per send would.
    let mut poll_send =
        |index: usize| poll_wait(&mut waits[index], &Waker::from(Arc::clone(&wakers[index])));
    assert!((0..4).all(|index| poll_send(index).is_pending()));
    // E = 40, e_min = 10 ⇒ τ = 30.
    assert_eq!(harness.channel.state().threshold, 30);

    // Drain to 25: the first send flushes on this one event; τ moves to 40 − 20.
    assert!(harness.drain(15));
    assert!(matches!(poll_send(0), Poll::Ready(Ok(()))));
    assert!((1..4).all(|index| poll_send(index).is_pending()));
    assert_eq!(harness.channel.state().threshold, 20);

    // One drain past two offsets settles both on the same event.
    assert!(harness.drain(20));
    assert!(matches!(poll_send(1), Poll::Ready(Ok(()))));
    assert!(matches!(poll_send(2), Poll::Ready(Ok(()))));
    assert!(poll_send(3).is_pending());
    assert_eq!(harness.channel.state().threshold, 0);

    assert!(harness.drain(5));
    assert!(matches!(poll_send(3), Poll::Ready(Ok(()))));
    // Every send was woken exactly once: by the event that flushed it.
    assert!(wakers
        .iter()
        .all(|waker| waker.0.load(Ordering::SeqCst) == 1));
    // τ rose with each registration, then fell with each flush of e_min.
    let mut armed = harness.channel.state().armed.clone();
    armed.dedup();
    assert_eq!(armed, vec![0, 10, 20, 30, 20, 0]);
}

/// Lost-wakeup rule, registration side: a drain that completed before the
/// registration is seen by the read that follows arming, without any event.
#[test]
fn test_flush_before_registration_is_observed_without_an_event() {
    let harness = Harness::new();
    let end_offset = harness.enqueue(8);
    // Any event of this drain finds nothing registered and settles nothing.
    harness.drain(8);
    let (mut wait, lease) = harness.tracker.track(end_offset);
    harness.run(lease);
    assert!(matches!(poll_now(&mut wait), Poll::Ready(Ok(()))));
}

/// Lost-wakeup rule, round side: an event that arrives after a round read `b`
/// but before it settled forces one more arm-and-read step.
#[test]
fn test_event_during_a_round_forces_a_rerun() {
    let harness = Harness::new();
    let (_, mut first) = harness.send(10);
    let end_offset = harness.enqueue(10);
    let (mut second, lease) = harness.tracker.track(end_offset);
    let lease = lease.expect("the tracker is idle, so registration starts a round");
    harness.channel.state().pause = Pause::AfterObserve;

    let mut round = pin!(lease.run(&harness.channel));
    let mut context = Context::from_waker(Waker::noop());
    // The round armed τ = 20 − 10 and read b = 20, then suspended.
    assert!(round.as_mut().poll(&mut context).is_pending());
    // Everything drains now; the crossing event finds the round running.
    assert!(harness.drain(20));
    assert!(
        poll_now(&mut first).is_pending(),
        "the running round owns settling"
    );
    while round.as_mut().poll(&mut context).is_pending() {}

    assert!(matches!(poll_now(&mut first), Poll::Ready(Ok(()))));
    assert!(matches!(poll_now(&mut second), Poll::Ready(Ok(()))));
}

/// Closure: a close before the flush yields `Err`, and so does every later send.
#[test]
fn test_close_before_flush_fails_pending_and_later_sends() {
    let harness = Harness::new();
    let (_, mut flushed) = harness.send(4);
    let (_, mut pending) = harness.send(4);
    let counter = Arc::new(CountingWaker::default());
    assert!(poll_wait(&mut pending, &Waker::from(Arc::clone(&counter))).is_pending());
    assert!(harness.drain(4));

    harness.tracker.close();

    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(poll_now(&mut flushed), Poll::Ready(Ok(()))));
    assert!(matches!(
        poll_now(&mut pending),
        Poll::Ready(Err(Error::MessageNotDelivered(_)))
    ));
    let (_, mut late) = harness.send(1);
    assert!(matches!(
        poll_now(&mut late),
        Poll::Ready(Err(Error::MessageNotDelivered(_)))
    ));
}

/// A dropped delivery future withdraws its offset from the threshold.
#[test]
fn test_dropped_wait_no_longer_bounds_the_threshold() {
    let harness = Harness::new();
    let (_, first) = harness.send(10);
    let (_, mut second) = harness.send(10);
    assert_eq!(harness.channel.state().threshold, 10);
    drop(first);
    // The drop re-arms nothing: τ stays at the dropped send's 10, above the 0
    // the remaining send needs, so the next event fires early, not late.
    assert_eq!(harness.channel.state().threshold, 10);
    assert!(harness.drain(10), "the stale threshold is crossed");
    // That event's round re-armed τ for the remaining send.
    assert_eq!(harness.channel.state().threshold, 0);
    assert!(poll_now(&mut second).is_pending());
    assert!(harness.drain(10));
    assert!(matches!(poll_now(&mut second), Poll::Ready(Ok(()))));
}

/// Soundness under a concurrent enqueue: a send that lands between the
/// round's read of `b` and its settlement advances `E` but not the `b` the
/// round saw. The round settles against the `E` it snapshotted before `b`, so
/// it cannot count that send's bytes as released.
#[test]
fn test_enqueue_during_a_round_fabricates_no_flush() {
    let harness = Harness::new();
    let (_, mut first) = harness.send(100);
    // 70 of the 100 bytes are released; τ = 0 is not crossed yet, so b = 30.
    assert!(!harness.drain(70));
    harness.channel.state().pause = Pause::EnqueueAfterObserve(50);

    harness.run(harness.tracker.notify());

    // E = 150 after the concurrent send, but b = 30 was read before it:
    // φ(150, 30, 100) would hold, while 30 bytes of the first send remain.
    assert!(poll_now(&mut first).is_pending());
    // The concurrent send registers after its enqueue, as the backend does;
    // its round re-arms τ = 150 − 100 against the advanced `E`.
    let (mut second, lease) = harness.tracker.track(150);
    harness.run(lease);
    assert_eq!(harness.channel.state().threshold, 50);
    assert!(harness.drain(30));
    assert!(matches!(poll_now(&mut first), Poll::Ready(Ok(()))));
    assert!(poll_now(&mut second).is_pending());
}

/// Liveness within one event across a registration during a round: the
/// registration's request forces a step against the advanced `E`, so τ tracks
/// the first send in the new `E`, and that send resolves on its own drain
/// rather than on the second send's.
#[test]
fn test_registration_during_a_round_rearms_against_the_new_counter() {
    let harness = Harness::new();
    let (_, mut first) = harness.send(100);
    let lease = harness.tracker.notify();
    let lease = lease.expect("the tracker is idle, so the event starts a round");
    harness.channel.state().pause = Pause::AfterObserve;
    let mut round = pin!(lease.run(&harness.channel));
    let mut context = Context::from_waker(Waker::noop());
    assert!(round.as_mut().poll(&mut context).is_pending());

    let end_offset = harness.enqueue(50);
    let (mut second, lease) = harness.tracker.track(end_offset);
    assert!(lease.is_none(), "the running round owns the registration");
    while round.as_mut().poll(&mut context).is_pending() {}

    // τ = 150 − 100: the first send's bytes leaving is a reported crossing.
    assert_eq!(harness.channel.state().threshold, 50);
    assert!(harness.drain(100));
    assert!(matches!(poll_now(&mut first), Poll::Ready(Ok(()))));
    assert!(poll_now(&mut second).is_pending());
}

/// A round abandoned before it ran (its executor dropped it) frees the round
/// slot: the next request starts a round, and the send still resolves.
#[test]
fn test_abandoned_round_lets_the_next_request_start_one() {
    let harness = Harness::new();
    let end_offset = harness.enqueue(3);
    let (mut wait, lease) = harness.tracker.track(end_offset);
    drop(lease);

    let lease = harness.tracker.notify();
    assert!(lease.is_some(), "the abandoned round returned to idle");
    harness.run(lease);
    assert!(poll_now(&mut wait).is_pending());
    assert!(harness.drain(3));
    assert!(matches!(poll_now(&mut wait), Poll::Ready(Ok(()))));
}
