#[cfg(not(target_family = "wasm"))]
use async_trait::async_trait;

use super::*;
#[cfg(not(target_family = "wasm"))]
use crate::core::callback::TransportCallback;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

#[cfg(not(target_family = "wasm"))]
type AdmittedPayloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[cfg(not(target_family = "wasm"))]
struct RecordingCallback {
    admitted: AdmittedPayloads,
}

#[cfg(not(target_family = "wasm"))]
struct InvalidRecordingCallback {
    invalid: Arc<AtomicUsize>,
}

#[cfg(not(target_family = "wasm"))]
struct PendingInvalidCallback;

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for RecordingCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (cid, payload, capacity) = message.into_parts();
        self.admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cid.to_owned(), payload.to_vec()));
        drop((payload, capacity));
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for InvalidRecordingCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.invalid.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for PendingInvalidCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        std::future::pending().await
    }
}

#[test]
fn raw_frame_capacity_releases_count_and_bytes_with_permit() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permit = capacity
        .try_acquire("peer-a", INBOUND_PEER_BYTE_CAPACITY)
        .expect("one peer's byte allowance must fit");
    assert!(capacity.try_acquire("peer-a", 1).is_none());
    drop(permit);
    assert!(capacity.try_acquire("peer-a", 1).is_some());
}

#[test]
fn one_peer_cannot_exhaust_another_peers_allowance() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permits = (0..INBOUND_PEER_FRAME_CAPACITY)
        .map(|_| capacity.try_acquire("noisy", 1))
        .collect::<Option<Vec<_>>>()
        .expect("one peer may use its complete frame allowance");

    assert!(capacity.try_acquire("noisy", 1).is_none());
    assert!(capacity.try_acquire("other", 1).is_some());
    drop(permits);
}

#[test]
fn raw_frame_capacity_is_shared_node_wide() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let permits = (0..INBOUND_FRAME_CAPACITY)
        .map(|index| {
            capacity.try_acquire(
                &format!("peer-{}", index / INBOUND_PEER_FRAME_CAPACITY),
                MAX_DATA_CHANNEL_MESSAGE_SIZE,
            )
        })
        .collect::<Option<Vec<_>>>()
        .expect("all node-wide frame slots must be available");

    assert!(capacity
        .try_acquire("extra", MAX_DATA_CHANNEL_MESSAGE_SIZE)
        .is_none());
    drop(permits);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn admission_dispatches_decoded_payload_once() {
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let capacity = Arc::new(InboundFrameCapacity::new());
    let callback = InnerTransportCallback::new_for_test(
        "peer",
        Box::new(RecordingCallback {
            admitted: Arc::clone(&admitted),
        }),
        Notifier::default(),
        Arc::clone(&capacity),
    );
    let data = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
        .expect("data frame must serialize");

    let frame = match callback.admit_inbound_frame(Bytes::from(data)) {
        InboundFrameAdmission::Admitted(frame) => frame,
        _ => panic!("data frame must be admitted"),
    };
    assert!(admitted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
    callback.handle_admitted_frame(frame).await;
    assert_eq!(
        admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[("peer".to_owned(), b"data".to_vec())]
    );
    assert!(matches!(
        callback.admit_inbound_frame(Bytes::from_static(b"malformed")),
        InboundFrameAdmission::Malformed(_)
    ));
}

#[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
#[tokio::test]
async fn prepare_inbound_frame_reports_remote_invalid_but_not_local_capacity() {
    let invalid = Arc::new(AtomicUsize::new(0));
    let callback = Arc::new(InnerTransportCallback::new_for_test(
        "peer",
        Box::new(InvalidRecordingCallback {
            invalid: Arc::clone(&invalid),
        }),
        Notifier::default(),
        Arc::new(InboundFrameCapacity::new()),
    ));
    let valid = Bytes::from(
        rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("valid frame must serialize"),
    );

    assert!(callback.prepare_inbound_frame(valid.clone()).is_some());
    assert!(callback
        .prepare_inbound_frame(Bytes::from_static(b"malformed"))
        .is_none());
    assert!(callback
        .prepare_inbound_frame(Bytes::from(vec![
            0;
            crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
                + 1
        ]))
        .is_none());

    let held = (0..INBOUND_PEER_FRAME_CAPACITY)
        .map(|_| {
            callback
                .prepare_inbound_frame(valid.clone())
                .expect("peer frame reservation must remain available")
        })
        .collect::<Vec<_>>();
    assert!(callback.prepare_inbound_frame(valid).is_none());

    for _ in 0..16 {
        if invalid.load(Ordering::Acquire) == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(invalid.load(Ordering::Acquire), 2);
    drop(held);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn admitted_frame_cannot_cross_callback_instances() {
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let capacity = Arc::new(InboundFrameCapacity::new());
    let callback = |cid| {
        InnerTransportCallback::new_for_test(
            cid,
            Box::new(RecordingCallback {
                admitted: Arc::clone(&admitted),
            }),
            Notifier::default(),
            Arc::clone(&capacity),
        )
    };
    let source = callback("source");
    let destination = callback("destination");
    let raw = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
        .expect("data frame must serialize");
    let frame = match source.admit_inbound_frame(Bytes::from(raw)) {
        InboundFrameAdmission::Admitted(frame) => frame,
        _ => panic!("source callback must admit the frame"),
    };

    destination.handle_admitted_frame(frame).await;

    assert!(admitted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
}

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

#[test]
fn borrowed_and_owned_transport_envelopes_share_the_complete_wire_schema() {
    let messages = [TransportMessage::Custom(Bytes::from_static(b"payload"))];

    for message in messages {
        let raw = rings_codec::serialize(&message).expect("transport frame must serialize");
        let (borrowed, remaining) =
            rings_codec::deserialize_prefix::<BorrowedTransportMessage>(&raw)
                .expect("borrowed envelope must decode every owned variant");
        assert!(remaining.is_empty());
        match (message, borrowed) {
            (TransportMessage::Custom(owned), BorrowedTransportMessage::Custom(view)) => {
                assert_eq!(owned.as_ref(), view);
            }
        }
    }
}

#[test]
fn inbound_data_channel_count_is_monotonic_and_bounded() {
    let admitted = AtomicUsize::new(0);
    for _ in 0..INBOUND_DATA_CHANNEL_CAPACITY {
        assert!(admit_inbound_data_channel(&admitted));
    }
    assert!(!admit_inbound_data_channel(&admitted));
}
