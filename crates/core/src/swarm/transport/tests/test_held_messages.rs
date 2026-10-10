//! Messages that arrive before this end admits the connection: held, delivered once in arrival
//! order by admission, and discarded with a cancelled connection.

use std::sync::Arc;

use super::pending_peer;
use super::transport_with_measure;
use super::CountingSwarmCallback;
use super::RecordingMeasure;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasurementEvent;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::tests::TEST_NETWORK_ID;

/// A verified message that arrives before this end admits the connection is held, not dropped,
/// and is delivered once by admission itself; a message after admission passes straight through.
#[tokio::test]
async fn test_pending_callback_messages_are_held_until_admission() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    let early = pending.custom_message_wire(&transport, b"message-before-admission")?;

    pending.receive(&early).await?;

    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 1);
    assert_eq!(app_callback.validates(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&pending.peer)?);

    pending.admit(&transport).await?;
    app_callback.wait_for_inbounds_at_least(1).await;

    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    let counters = measure.snapshot_counters()?;
    assert!(counters.contains(&(pending.peer, MeasurementEvent::Connected)));
    assert!(counters
        .iter()
        .any(|(peer, event)| *peer == pending.peer
            && matches!(event, MeasurementEvent::Received { .. })));
    assert!(transport.has_active_connection(pending.peer));

    let late = pending.custom_message_wire(&transport, b"message-after-admission")?;
    pending.receive(&late).await?;
    assert_eq!(app_callback.inbounds(), 2);
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"message-before-admission".to_vec(),
        b"message-after-admission".to_vec(),
    ]);

    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (order): held messages are delivered in arrival order, ahead of anything that arrives
/// after admission.
#[tokio::test]
async fn test_held_messages_are_delivered_in_arrival_order() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;

    pending
        .receive(&pending.custom_message_wire(&transport, b"first")?)
        .await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"second")?)
        .await?;
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 2);
    assert_eq!(app_callback.inbounds(), 0);

    pending.admit(&transport).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"third")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(3).await;

    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"first".to_vec(),
        b"second".to_vec(),
        b"third".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// A message from a peer the handshake does not belong to cancels the handshake, and the frames
/// held for it are discarded with it.
#[tokio::test]
async fn test_held_messages_are_discarded_when_the_pending_connection_is_cancelled() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"held")?)
        .await?;
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 1);

    let stranger_key = SecretKey::random();
    let stranger: Did = stranger_key.address().into();
    let stranger_session = DelegateeKey::new_with_seckey(&stranger_key)?;
    let intrusion = MessagePayload::new_send(
        Message::custom(b"stranger")?,
        MessageSigner::new(&stranger_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    pending
        .callback
        .on_admitted_message_for_test(&stranger.to_string(), &intrusion)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 0);
    assert!(!transport.has_connection_attempt(pending.peer)?);
    assert_eq!(app_callback.inbounds(), 0);
    Ok(())
}
