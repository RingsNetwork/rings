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
