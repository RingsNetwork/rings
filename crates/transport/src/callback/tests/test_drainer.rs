//! A lane drainer that ends mid-dispatch gives its lane up.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use super::wire;
use super::InboundFrameAdmission;
use super::InnerTransportCallback;
use super::NodeReceiveLoad;
use crate::core::callback::AdmittedInboundMessage;
use crate::core::callback::TransportCallback;
use crate::core::pool::ChannelLane;
use crate::core::pool::DATA_CHANNEL_POOL_SIZE;
use crate::core::transport::TransportMessage;
use crate::notifier::Notifier;

/// The payload whose handler panics.
const PANICKING: &[u8] = b"panic";

/// A protocol callback that panics on [`PANICKING`], announcing the panic just before it, and
/// records every other payload it takes over.
struct PanickingCallback {
    /// Notified right before the panic.
    panicking: Arc<Notify>,
    /// The payloads taken over.
    seen: mpsc::UnboundedSender<Bytes>,
}

#[async_trait]
impl TransportCallback for PanickingCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (_, payload, _lease) = message.into_parts();
        if payload.as_ref() == PANICKING {
            self.panicking.notify_one();
            panic!("the protocol handler fails mid-dispatch");
        }
        self.seen.send(payload).ok();
        Ok(())
    }
}

/// Admit the custom frame of `payload` on `lane` and queue it for dispatch.
fn dispatch(callback: &Arc<InnerTransportCallback>, payload: &'static [u8], lane: ChannelLane) {
    let raw = wire(&TransportMessage::Custom(Bytes::from_static(payload)));
    let InboundFrameAdmission::Admitted(frame) = callback.admit_inbound_frame(raw, lane) else {
        panic!("a frame within the window must be admitted");
    };
    callback.dispatch_admitted_frame(frame, lane);
}

/// A drainer unwound by a panicking handler gives its lane up: the frames queued behind the
/// panic return their credit, and the lane's next frame is handed over by a new drainer.
///
/// The runtime is single-threaded, so once the test runs again after the announcement, the
/// drainer's poll, and with it the unwinding, has completed. The timeout is a hang guard.
#[tokio::test]
async fn test_a_drainer_ended_mid_dispatch_gives_its_lane_up() {
    let panicking = Arc::new(Notify::new());
    let (seen, mut handed) = mpsc::unbounded_channel();
    let callback = Arc::new(InnerTransportCallback::new(
        "peer",
        Box::new(PanickingCallback {
            panicking: Arc::clone(&panicking),
            seen,
        }),
        Notifier::default(),
        NodeReceiveLoad::new(),
    ));
    let lane = ChannelLane::new(1);
    dispatch(&callback, PANICKING, lane);
    dispatch(&callback, b"queued behind", lane);
    dispatch(&callback, b"queued last", lane);

    panicking.notified().await;
    assert_eq!(
        callback.link_credit().occupancy(),
        [0; DATA_CHANNEL_POOL_SIZE as usize]
    );

    dispatch(&callback, b"after", lane);
    let after = tokio::time::timeout(Duration::from_secs(30), handed.recv())
        .await
        .expect("the lane's next frame is handed over")
        .expect("the callback is alive");
    assert_eq!(after.as_ref(), b"after");
    assert!(
        handed.try_recv().is_err(),
        "the abandoned frames are dropped"
    );
}
