use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::*;

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn invalid_frame_reports_use_one_coalesced_worker() {
    let invalid = Arc::new(AtomicUsize::new(0));
    let callback = InnerTransportCallback::new_for_test(
        "peer",
        Box::new(InvalidRecordingCallback {
            invalid: Arc::clone(&invalid),
        }),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );

    for index in 0..64 {
        assert_eq!(callback.queue_invalid_inbound_frame(), index == 0);
    }
    callback.drain_invalid_inbound_frames().await;

    assert_eq!(invalid.load(Ordering::Acquire), 64);
    assert!(callback.queue_invalid_inbound_frame());
    callback.drain_invalid_inbound_frames().await;
    assert_eq!(invalid.load(Ordering::Acquire), 65);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn cancelling_invalid_frame_worker_discards_backlog_and_allows_replacement() {
    let callback = InnerTransportCallback::new_for_test(
        "peer",
        Box::new(PendingInvalidCallback),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    assert!(callback.queue_invalid_inbound_frame());
    let mut drain = Box::pin(callback.drain_invalid_inbound_frames());
    assert!(futures::poll!(&mut drain).is_pending());
    drop(drain);

    assert_eq!(callback.pending_invalid_frame_count_for_test(), 0);
    assert!(callback.queue_invalid_inbound_frame());
    assert_eq!(callback.pending_invalid_frame_count_for_test(), 1);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn invalid_frame_backlog_is_bounded_and_yields_between_quanta() {
    let invalid = Arc::new(AtomicUsize::new(0));
    let callback = InnerTransportCallback::new_for_test(
        "peer",
        Box::new(InvalidRecordingCallback {
            invalid: Arc::clone(&invalid),
        }),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    );
    for _ in 0..INVALID_FRAME_REPORT_BACKLOG_CAPACITY.saturating_add(32) {
        callback.queue_invalid_inbound_frame();
    }
    assert_eq!(
        callback.pending_invalid_frame_count_for_test(),
        INVALID_FRAME_REPORT_BACKLOG_CAPACITY
    );

    let mut drain = Box::pin(callback.drain_invalid_inbound_frames());
    assert!(futures::poll!(&mut drain).is_pending());
    assert_eq!(
        invalid.load(Ordering::Acquire),
        INVALID_FRAME_REPORT_QUANTUM
    );
    drain.await;

    assert_eq!(
        invalid.load(Ordering::Acquire),
        INVALID_FRAME_REPORT_BACKLOG_CAPACITY
    );
    assert_eq!(callback.pending_invalid_frame_count_for_test(), 0);
}
