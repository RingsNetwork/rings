use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;

use super::*;

#[tokio::test]
async fn closed_channel_bypasses_a_pending_debounce_delay() {
    let (mut sender, mut receiver) = mpsc::channel(1);
    sender
        .try_send(())
        .unwrap_or_else(|error| panic!("initial wake must fit: {error}"));
    assert_eq!(receiver.next().await, Some(()));
    let wait = wait_for_debounce_or_close_with_delay(
        &mut receiver,
        futures::future::pending::<Result<(), MeasureRuntimeError>>(),
    );
    tokio::pin!(wait);
    drop(sender);
    assert!(!tokio::time::timeout(Duration::from_millis(10), wait)
        .await
        .unwrap_or_else(|_| panic!("channel close must bypass the pending debounce")));
}

#[tokio::test]
async fn timer_failure_waits_for_a_new_wake_instead_of_hot_retrying() {
    let (mut sender, mut receiver) = mpsc::channel(1);
    let wait = wait_for_retry_or_close_with_delay(
        &mut receiver,
        futures::future::ready(Err(MeasureRuntimeError::Timer("fixture".to_string()))),
    );
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), wait.as_mut())
            .await
            .is_err(),
        "timer failure must park on the next semantic wake"
    );
    sender
        .try_send(())
        .unwrap_or_else(|error| panic!("retry wake must fit: {error}"));
    assert!(wait.await);
}

#[tokio::test]
async fn wake_observed_before_timer_failure_still_triggers_retry() {
    let (mut sender, mut receiver) = mpsc::channel(1);
    let (delay_sender, delay_receiver) = futures::channel::oneshot::channel();
    let delay = async move {
        delay_receiver
            .await
            .unwrap_or_else(|_| Err(MeasureRuntimeError::FlushTaskStopped))
    };
    let wait = wait_for_retry_or_close_with_delay(&mut receiver, delay);
    tokio::pin!(wait);
    sender
        .try_send(())
        .unwrap_or_else(|error| panic!("retry wake must fit: {error}"));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), wait.as_mut())
            .await
            .is_err(),
        "the wait must consume the wake while its timer remains pending"
    );
    delay_sender
        .send(Err(MeasureRuntimeError::Timer("fixture".to_string())))
        .unwrap_or_else(|_| panic!("timer result receiver must remain live"));
    assert!(wait.await, "the consumed wake must authorize one retry");
}
