use rings_transport::callback::AdmittedInboundFrame;
use rings_transport::callback::InboundFrameAdmission;
use rings_transport::callback::InnerTransportCallback;
use rings_transport::core::callback::AdmittedInboundMessage;
use rings_transport::core::transport::TransportMessage;
use rings_transport::notifier::Notifier;

use super::*;

#[derive(Default)]
struct BlockingValidateSwarmCallback {
    started: Notify,
    release: Notify,
}

#[async_trait]
impl SwarmCallback for BlockingValidateSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.started.notify_one();
        self.release.notified().await;
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
    match callback.admit_inbound_frame(raw) {
        InboundFrameAdmission::Admitted(frame) => Ok(frame),
        _ => Err(Error::InvalidMessage(
            "valid raw transport frame was not admitted".to_string(),
        )),
    }
}

fn retain_remaining_raw_capacity(
    callback: &InnerTransportCallback,
    raw: &bytes::Bytes,
) -> Result<Vec<AdmittedInboundFrame>> {
    let mut retained = Vec::new();
    loop {
        match callback.admit_inbound_frame(raw.clone()) {
            InboundFrameAdmission::Admitted(frame) => retained.push(frame),
            InboundFrameAdmission::CapacityExceeded => return Ok(retained),
            _ => {
                return Err(Error::InvalidMessage(
                    "valid raw transport frame became invalid".to_string(),
                ));
            }
        }
    }
}

async fn wait_for_raw_capacity_release(
    callback: &InnerTransportCallback,
    raw: &bytes::Bytes,
) -> Result<AdmittedInboundFrame> {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match callback.admit_inbound_frame(raw.clone()) {
                InboundFrameAdmission::Admitted(frame) => return Ok(frame),
                InboundFrameAdmission::CapacityExceeded => tokio::task::yield_now().await,
                _ => {
                    return Err(Error::InvalidMessage(
                        "valid raw transport frame became invalid".to_string(),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| Error::InvalidMessage("raw transport capacity was not released".to_string()))?
}

#[tokio::test]
async fn test_raw_transport_lease_is_held_until_core_capacity_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = SessionSk::new_with_seckey(&peer_key)?;
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
    let transport_callback = Arc::new(InnerTransportCallback::for_transport(
        &transport.transport,
        &peer.to_string(),
        Box::new(SharedCoreCallback(Arc::clone(&core_callback))),
        Notifier::default(),
    ));
    let frame = admit_raw_frame(&transport_callback, raw.clone())?;
    let dispatch_callback = Arc::clone(&transport_callback);
    let mut dispatch = Box::pin(async move {
        dispatch_callback.handle_admitted_frame(frame).await;
    });

    assert!(futures::poll!(&mut dispatch).is_pending());
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 1);
    let dispatch = tokio::spawn(dispatch);
    let retained = retain_remaining_raw_capacity(&transport_callback, &raw)?;
    assert!(!retained.is_empty());
    assert!(matches!(
        transport_callback.admit_inbound_frame(raw.clone()),
        InboundFrameAdmission::CapacityExceeded
    ));

    drop(admission_blocker);
    tokio::time::timeout(Duration::from_secs(1), application.started.notified())
        .await
        .map_err(|_| Error::InvalidMessage("core processing did not start".to_string()))?;
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 1);
    let released = wait_for_raw_capacity_release(&transport_callback, &raw).await?;
    drop((released, retained));
    application.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), dispatch)
        .await
        .map_err(|_| Error::InvalidMessage("transport dispatch did not stop".to_string()))?
        .map_err(|_| Error::InvalidMessage("transport dispatch task panicked".to_string()))?;
    assert_eq!(core_callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}
