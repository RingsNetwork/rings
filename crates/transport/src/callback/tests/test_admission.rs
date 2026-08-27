use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;

use super::*;
use crate::core::transport::TransportMessage;

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_admission_dispatches_decoded_payload_once() {
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
async fn test_prepare_inbound_frame_reports_remote_invalid_but_not_local_capacity() {
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
async fn test_admitted_frame_cannot_cross_callback_instances() {
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
