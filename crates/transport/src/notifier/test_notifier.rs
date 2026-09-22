use super::*;

#[tokio::test]
async fn test_notifier() {
    let notifier = Notifier::default();
    notifier.set_timeout(1);

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
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            notifier_clone.await;
        }));
    }

    futures::future::join_all(jobs).await;
    notifier.await;
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
