use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use super::InboundFrameAdmission;
use super::InboundFrameCapacity;
use super::InnerTransportCallback;
use super::INBOUND_PEER_FRAME_CAPACITY;
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::TransportCallback;
use crate::core::transport::TransportMessage;
use crate::notifier::Notifier;

struct PendingAfterCapacityHandoff;

struct DropObservedBytes {
    data: Vec<u8>,
    dropped: Arc<AtomicBool>,
}

impl AsRef<[u8]> for DropObservedBytes {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref()
    }
}

impl Drop for DropObservedBytes {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

#[async_trait]
impl TransportCallback for PendingAfterCapacityHandoff {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (_, payload, capacity) = message.into_parts();
        drop((payload, capacity));
        std::future::pending().await
    }
}

#[tokio::test]
async fn test_downstream_capacity_handoff_releases_raw_limit_before_callback_completion() {
    let capacity = Arc::new(InboundFrameCapacity::new());
    let callback = InnerTransportCallback::new_for_test(
        "peer",
        Box::new(PendingAfterCapacityHandoff),
        Notifier::default(),
        Arc::clone(&capacity),
    );
    let raw_dropped = Arc::new(AtomicBool::new(false));
    let raw = Bytes::from_owner(DropObservedBytes {
        data: rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("data frame must serialize"),
        dropped: raw_dropped.clone(),
    });
    let frame = match callback.admit_inbound_frame(raw) {
        InboundFrameAdmission::Admitted(frame) => frame,
        _ => panic!("data frame must be admitted"),
    };
    let mut dispatch = Box::pin(callback.handle_admitted_frame(frame));

    assert!(futures::poll!(&mut dispatch).is_pending());
    assert!(raw_dropped.load(Ordering::Acquire));
    let permits = (0..INBOUND_PEER_FRAME_CAPACITY)
        .map(|_| capacity.try_acquire("peer", 1))
        .collect::<Option<Vec<_>>>()
        .expect("raw capacity must be free while the downstream callback remains pending");

    assert!(capacity.try_acquire("peer", 1).is_none());
    drop(permits);
}
