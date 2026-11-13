use std::future::Future;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use super::*;

/// Upper bound for a wait that must end by an event, never a real timeout.
const EVENT_WAIT_BOUND: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn test_wake_releases_every_waiter_including_late_ones() {
    let notifier = Notifier::default();
    let early = (0..3)
        .map(|_| tokio::spawn(notifier.clone()))
        .collect::<Vec<_>>();

    notifier.wake();
    let late = (0..3)
        .map(|_| tokio::spawn(notifier.clone()))
        .collect::<Vec<_>>();

    for waiter in early.into_iter().chain(late) {
        waiter.await.expect("notifier waiter task must complete");
    }
}

#[tokio::test]
async fn test_notifier_is_pending_before_wake_and_ready_after_wake() {
    let notifier = Notifier::default();
    let mut waiter = Box::pin(notifier.clone());
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);

    assert!(matches!(waiter.as_mut().poll(&mut context), Poll::Pending));
    notifier.wake();
    assert!(matches!(
        waiter.as_mut().poll(&mut context),
        Poll::Ready(())
    ));
    let mut late_waiter = Box::pin(notifier);
    assert!(matches!(
        late_waiter.as_mut().poll(&mut context),
        Poll::Ready(())
    ));
}

/// Law: a timeout ends only the wait; the notifier itself stays unwoken.
#[tokio::test]
async fn test_timeout_ends_the_wait_without_waking_the_notifier() {
    let notifier = Notifier::default();

    notifier
        .notified_within(Duration::from_millis(1))
        .await
        .expect("native timers never fail");

    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Box::pin(notifier).as_mut().poll(&mut context),
        Poll::Pending
    ));
}

/// Law: a wake ends a pending wait long before its timeout.
#[tokio::test]
async fn test_wake_ends_a_wait_before_its_timeout() {
    let notifier = Notifier::default();
    let waiting = notifier.clone();
    let wait = tokio::spawn(async move { waiting.notified_within(EVENT_WAIT_BOUND).await });

    notifier.wake();

    wait.await
        .expect("waiter task must complete")
        .expect("native timers never fail");
}

/// The composed wait spawns nothing, so it completes on a thread with no entered runtime.
#[test]
fn test_wait_needs_no_entered_runtime() {
    let worker = std::thread::spawn(|| {
        assert!(tokio::runtime::Handle::try_current().is_err());
        futures::executor::block_on(Notifier::default().notified_within(Duration::from_millis(1)))
    });

    worker
        .join()
        .expect("runtime-less waiter thread")
        .expect("native timers never fail");
}

/// Invariant: repeated timed-out waits from one task keep a single registered waker.
#[test]
fn test_repeated_waits_from_one_task_register_one_waker() {
    let notifier = Notifier::default();

    for _ in 0..3 {
        futures::executor::block_on(notifier.notified_within(Duration::from_millis(1)))
            .expect("native timers never fail");
    }

    assert_eq!(notifier.state().wakers.len(), 1);
}

/// Law: `Notifier::wake` drops its internal mutex before invoking registered wakers, so an
/// inline waker that synchronously re-enters `Notifier::poll` and `Notifier::wake` on the same
/// thread completes without self-deadlocking.
#[test]
fn test_wake_invokes_inline_reentrant_waker_outside_lock() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct ReentrantInlineWaker {
        notifier: Notifier,
        polled_ready: AtomicBool,
    }

    impl futures::task::ArcWake for ReentrantInlineWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            let noop = futures::task::noop_waker();
            let mut cx = Context::from_waker(&noop);
            let mut waiter = Box::pin(arc_self.notifier.clone());
            if matches!(waiter.as_mut().poll(&mut cx), Poll::Ready(())) {
                arc_self.polled_ready.store(true, Ordering::SeqCst);
            }
            arc_self.notifier.wake();
        }
    }

    let notifier = Notifier::default();
    let inline = Arc::new(ReentrantInlineWaker {
        notifier: notifier.clone(),
        polled_ready: AtomicBool::new(false),
    });
    let waker = futures::task::waker_ref(&inline);
    let mut cx = Context::from_waker(&waker);
    let mut waiter = Box::pin(notifier.clone());

    assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Pending));
    notifier.wake();
    assert!(inline.polled_ready.load(Ordering::SeqCst));
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[tokio::test]
async fn test_wait_for_data_channel_open_reports_unavailable_on_terminal_wake() {
    use crate::core::transport::ConnectionStateCell;
    use crate::core::transport::WebrtcConnectionState;

    let state = ConnectionStateCell::new();
    let notifier = Notifier::default();
    let waiter_state = state.clone();
    let waiter_notifier = notifier.clone();
    let wait = tokio::spawn(async move {
        wait_for_data_channel_open(move || waiter_state.snapshot(), &waiter_notifier, 8).await
    });

    tokio::task::yield_now().await;
    state.observe_webrtc(WebrtcConnectionState::Failed);
    notifier.wake();

    let err = wait
        .await
        .expect("wait task completes")
        .expect_err("terminal connection must fail wait_for_data_channel_open");
    assert!(
        matches!(err, Error::DataChannelOpen(ref msg) if msg == "Connection unavailable"),
        "expected Connection unavailable, got {err:?}"
    );
}
