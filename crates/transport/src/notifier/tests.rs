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
