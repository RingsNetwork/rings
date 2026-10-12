use rings_transport::callback::AdmittedInboundFrame;
use rings_transport::callback::InboundFrameAdmission;
use rings_transport::callback::InnerTransportCallback;
use rings_transport::callback::NodeReceiveLoad;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::credit::LANE_CREDIT_WINDOW;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::custom_frame_wire;
use rings_transport::notifier::Notifier;

use super::*;
use crate::message::MessageCategory;

/// The connection lane this test's application frames travel on: the receiver refuses a frame
/// on any lane but its category's (one lane per class).
const APPLICATION_CHANNEL: ChannelLane = MessageCategory::Application.channel_lane();

struct SharedCoreCallback(Arc<SwarmTransportCallback>);

#[async_trait]
impl TransportCallback for SharedCoreCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        TransportCallback::on_admitted_message(self.0.as_ref(), message).await
    }
}

fn admit_raw_frame(
    callback: &InnerTransportCallback,
    raw: bytes::Bytes,
) -> Result<AdmittedInboundFrame> {
    match callback.admit_inbound_frame(raw, APPLICATION_CHANNEL) {
        InboundFrameAdmission::Admitted(frame) => Ok(frame),
        _ => Err(Error::InvalidMessage(
            "valid raw transport frame was not admitted".to_string(),
        )),
    }
}

/// Await the core handoff that releases the dispatched frame's raw lease, then witness the
/// release through the credit law: with the rest of the window held, releasing half a window
/// less one of the held frames completes a batch, and so advertises more credit, only if the
/// dispatched frame's release was counted. The lease drop releases synchronously before the
/// handoff is published, so no retry is needed.
async fn wait_for_raw_capacity_release(
    core_callback: &SwarmTransportCallback,
    callback: &InnerTransportCallback,
    raw: &bytes::Bytes,
    held: &mut Vec<AdmittedInboundFrame>,
) -> Result<AdmittedInboundFrame> {
    core_callback
        .inbound_for_test()
        .await_handoffs_for_test(|handoffs| handoffs >= 1)
        .await;
    let batch = usize::try_from(LANE_CREDIT_WINDOW / 2)
        .map_err(|_| Error::InvalidMessage("the credit batch must fit usize".to_string()))?;
    held.truncate(held.len().saturating_sub(batch.saturating_sub(1)));
    match callback.admit_inbound_frame(raw.clone(), APPLICATION_CHANNEL) {
        InboundFrameAdmission::Admitted(frame) => Ok(frame),
        InboundFrameAdmission::CreditExceeded { .. } => Err(Error::InvalidMessage(
            "the lane's credit was not released at the core handoff".to_string(),
        )),
        _ => Err(Error::InvalidMessage(
            "valid raw transport frame became invalid".to_string(),
        )),
    }
}

#[tokio::test]
async fn test_raw_transport_lease_is_held_until_core_capacity_admission() -> Result<()> {
    let transport = default_transport()?;
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let payload = peer.custom_message_wire(&transport, b"transport-capacity-handoff")?;
    let raw = bytes::Bytes::from(
        custom_frame_wire(payload).map_err(|error| Error::InvalidMessage(error.to_string()))?,
    );
    let (application, lane_hold) =
        ApplicationRecorder::attach_holding_application_lane(&transport)?;
    let core_callback = Arc::clone(&peer.callback);
    let admission_blocker = core_callback
        .inbound_for_test()
        .hold_application_admission_for_test()?;
    let transport_callback = Arc::new(InnerTransportCallback::new(
        &peer.peer.to_string(),
        Box::new(SharedCoreCallback(Arc::clone(&core_callback))),
        Notifier::default(),
        NodeReceiveLoad::new(),
    ));
    let frame = admit_raw_frame(&transport_callback, raw.clone())?;
    // Fill the rest of the lane's window, so only the dispatched frame's release can admit more.
    let mut window = (1..LANE_CREDIT_WINDOW)
        .map(|_| admit_raw_frame(&transport_callback, raw.clone()))
        .collect::<Result<Vec<_>>>()?;
    let dispatch_callback = Arc::clone(&transport_callback);
    let dispatch = tokio::spawn(async move {
        dispatch_callback.handle_admitted_frame(frame).await;
    });

    // The raw lease is released at the core handoff, once core capacity admits
    // the decoded frame and before the logical stage sees it: the application
    // lane is still held, so the frame has not entered that stage.
    let released =
        wait_for_raw_capacity_release(&core_callback, &transport_callback, &raw, &mut window)
            .await?;
    assert_eq!(
        core_callback.inbound_for_test().admitted_count_for_test(),
        1
    );
    assert_eq!(application.observed(), 0);
    drop((released, window));

    // Admitted, the frame enters the logical stage and waits at its held lane, still holding
    // its core permit: only the core permit outlives the handoff.
    drop(admission_blocker);
    application.next_tapped().await?;
    assert_eq!(
        core_callback.inbound_for_test().admitted_count_for_test(),
        1
    );
    drop(lane_hold);
    dispatch
        .await
        .map_err(|_| Error::InvalidMessage("transport dispatch task panicked".to_string()))?;
    assert_eq!(
        core_callback.inbound_for_test().admitted_count_for_test(),
        0
    );
    Ok(())
}
