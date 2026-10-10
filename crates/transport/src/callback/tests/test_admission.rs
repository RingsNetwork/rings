#[cfg(feature = "tokio")]
use std::sync::atomic::AtomicUsize;
#[cfg(feature = "tokio")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;

use super::*;
#[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::transport::TransportMessage;

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_admission_dispatches_decoded_payload_once() {
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let callback = InnerTransportCallback::new(
        "peer",
        Box::new(RecordingCallback {
            admitted: Arc::clone(&admitted),
        }),
        Notifier::default(),
        NodeReceiveLoad::new(),
    );
    let data = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
        .expect("data frame must serialize");

    let frame = match callback.admit_inbound_frame(Bytes::from(data), ChannelLane::default()) {
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
        callback.admit_inbound_frame(Bytes::from_static(b"malformed"), ChannelLane::default()),
        InboundFrameAdmission::Malformed(_)
    ));
}

/// A malformed frame, an oversized one and one beyond the advertised credit are each refused
/// and reported to the callback as invalid.
#[cfg(all(not(target_family = "wasm"), feature = "tokio"))]
#[tokio::test]
async fn test_prepare_inbound_frame_reports_malformed_oversized_and_over_credit_frames() {
    let invalid = Arc::new(AtomicUsize::new(0));
    let callback = Arc::new(InnerTransportCallback::new(
        "peer",
        Box::new(InvalidRecordingCallback {
            invalid: Arc::clone(&invalid),
        }),
        Notifier::default(),
        NodeReceiveLoad::new(),
    ));
    let valid = Bytes::from(
        rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("valid frame must serialize"),
    );

    let lane = ChannelLane::default();
    assert!(callback
        .prepare_inbound_frame(Bytes::from_static(b"malformed"), lane)
        .is_none());
    assert!(callback
        .prepare_inbound_frame(
            Bytes::from(vec![
                0;
                crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
                    + 1
            ]),
            lane
        )
        .is_none());

    // A whole window of the lane is admitted; the frame beyond it broke flow control.
    let held = (0..LANE_CREDIT_WINDOW)
        .map(|_| {
            callback
                .prepare_inbound_frame(valid.clone(), lane)
                .expect("a frame within the lane's credit must be admitted")
        })
        .collect::<Vec<_>>();
    assert!(callback.prepare_inbound_frame(valid, lane).is_none());

    // The reports wait in the coalesced backlog; draining it here, before the spawned worker
    // first runs on this single-threaded runtime, delivers every one of them.
    callback.drain_invalid_inbound_frames().await;
    assert_eq!(invalid.load(Ordering::Acquire), 3);
    drop(held);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test]
async fn test_admitted_frame_cannot_cross_callback_instances() {
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let callback = |cid| {
        InnerTransportCallback::new(
            cid,
            Box::new(RecordingCallback {
                admitted: Arc::clone(&admitted),
            }),
            Notifier::default(),
            NodeReceiveLoad::new(),
        )
    };
    // Distinct identities must reject both another peer and a replacement callback
    // for the same peer; the redundant peer-string comparison cannot prove this.
    for destination_id in ["source", "destination"] {
        let source = callback("source");
        let destination = callback(destination_id);
        let raw = rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("data frame must serialize");
        let frame = match source.admit_inbound_frame(Bytes::from(raw), ChannelLane::default()) {
            InboundFrameAdmission::Admitted(frame) => frame,
            _ => panic!("source callback must admit the frame"),
        };

        destination.handle_admitted_frame(frame).await;
        assert!(admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
    }
}
