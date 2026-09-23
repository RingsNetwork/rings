use std::future::Future;
use std::task::Context;
use std::task::Poll;

use super::*;

#[tokio::test]
async fn test_notifier() {
    let notifier = Notifier::default();
    notifier.set_timeout_ms(100);

    let mut jobs = vec![];

    // Await three times.
    for _ in 0..3 {
        let notifier_clone = notifier.clone();
        jobs.push(tokio::spawn(async move {
            notifier_clone.await;
        }));
    }

    // Await three times after wake.
    for _ in 0..3 {
        let notifier_clone = notifier.clone();
        jobs.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            notifier_clone.await;
        }));
    }

    let results = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        futures::future::join_all(jobs),
    )
    .await
    .expect("all notifier waiters must finish within 500 ms");
    for result in results {
        assert!(
            result.is_ok(),
            "notifier waiter task must complete successfully"
        );
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), notifier)
            .await
            .is_ok()
    );
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

/// The fallback timer must wake an executor without any entered Tokio runtime.
#[cfg(not(feature = "native-webrtc"))]
#[test]
fn test_fallback_timeout_wakes_without_entered_runtime() {
    // A bounded channel lets the test fail instead of hanging if the timer loses a wake.
    let (completed, observed) = std::sync::mpsc::channel();
    // The plain thread has no runtime context, even when Tokio is a dependency.
    let worker = std::thread::spawn(move || {
        // This notifier owns the fallback scheduler registration until it wakes.
        let notifier = Notifier::default();
        assert!(tokio::runtime::Handle::try_current().is_err());
        notifier.set_timeout_ms(1);
        futures::executor::block_on(notifier);
        completed
            .send(())
            .expect("completion observer must remain alive");
    });
    observed
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("fallback scheduler must wake without Tokio");
    worker.join().expect("fallback timer worker must complete");
}
