use rings_transport::callback::AdmittedInboundFrame;
use rings_transport::callback::InboundFrameAdmission;
use rings_transport::callback::InnerTransportCallback;
use rings_transport::callback::NodeReceiveLoad;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::credit::LANE_CREDIT_WINDOW;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::TransportMessage;
use rings_transport::notifier::Notifier;

use super::*;

#[derive(Default)]
struct BlockingValidateSwarmCallback {
    started: TestLatch,
    release: TestLatch,
}

#[async_trait]
impl SwarmCallback for BlockingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.started.set();
        self.release.wait().await;
        Ok(())
    }
}

struct SharedCoreCallback(Arc<InnerSwarmCallback>);

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
    match callback.admit_inbound_frame(raw, ChannelLane::default()) {
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
    core_callback: &InnerSwarmCallback,
    callback: &InnerTransportCallback,
    raw: &bytes::Bytes,
    held: &mut Vec<AdmittedInboundFrame>,
) -> Result<AdmittedInboundFrame> {
    core_callback
        .await_inbound_handoffs_for_test(|handoffs| handoffs >= 1)
        .await;
    let batch = usize::try_from(LANE_CREDIT_WINDOW / 2)
        .map_err(|_| Error::InvalidMessage("the credit batch must fit usize".to_string()))?;
    held.truncate(held.len().saturating_sub(batch.saturating_sub(1)));
    match callback.admit_inbound_frame(raw.clone(), ChannelLane::default()) {
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
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let payload = local_wire(
        Message::custom(b"transport-capacity-handoff")?,
        &session,
        transport.dht.did,
    )?;
    let raw = bytes::Bytes::from(
        rings_codec::serialize(&TransportMessage::Custom(payload))
            .map_err(|error| Error::InvalidMessage(error.to_string()))?,
    );
    let application = Arc::new(BlockingValidateSwarmCallback::default());
    let core_callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        application.clone(),
    ));
    let admission_blocker = core_callback.hold_application_admission_for_test()?;
    let transport_callback = Arc::new(InnerTransportCallback::new(
        &peer.to_string(),
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
    // the decoded frame and before the application sees it: the application
    // lane is still held, so `on_validate` has not started.
    let released =
        wait_for_raw_capacity_release(&core_callback, &transport_callback, &raw, &mut window)
            .await?;
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 1);
    assert!(!application.started.is_set());
    drop((released, window));

    drop(admission_blocker);
    application.started.wait().await;
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 1);
    application.release.set();
    dispatch
        .await
        .map_err(|_| Error::InvalidMessage("transport dispatch task panicked".to_string()))?;
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}
