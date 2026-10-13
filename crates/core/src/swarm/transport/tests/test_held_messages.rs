//! Messages that arrive while a connection is pending: held until admission, delivered in
//! arrival order, or discarded with the pending connection.

use std::sync::Arc;

use bytes::Bytes;
use rings_transport::core::callback::TransportCallback;
use rings_transport::core::transport::WebrtcConnectionState;
#[cfg(not(target_family = "wasm"))]
use tracing_test::traced_test;

use super::super::outbound::TransferDemand;
use super::super::OUTBOUND_DATA_TRANSFER_CAPACITY;
use super::open_dummy_data_channel_before_ice_connected;
use super::pending_peer;
use super::transport_with_measure;
use super::BlockingConnectMeasure;
use super::CountingSwarmCallback;
use super::NoopSwarmCallback;
use super::RecordingMeasure;
use crate::chunk::Chunk;
use crate::chunk::ChunkMeta;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasurementEvent;
use crate::message::Message;
use crate::message::MessageCategory;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::swarm::callback::max_on_message_recursion_depth_for_test;
use crate::swarm::callback::reset_on_message_recursion_depth_for_test;
use crate::swarm::callback::InnerSwarmCallback;
use crate::tests::TEST_NETWORK_ID;

/// A verified message that arrives before this end admits the connection is held, not dropped,
/// and is delivered once by admission itself; a message after admission passes straight through.
#[cfg(feature = "dummy")]
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
#[cfg(feature = "dummy")]
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
#[cfg(feature = "dummy")]
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

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_nested_reassembled_chunk_is_rejected_without_recursive_callback_entry() -> Result<()>
{
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    let mut current: Bytes = MessagePayload::new_send(
        Message::custom(b"mailbox-drained")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    for _ in 0..2 {
        let chunk = Chunk {
            chunk: [0, 1],
            data: current,
            meta: ChunkMeta::default(),
        };
        current = MessagePayload::new_send(
            Message::Chunk(chunk),
            MessageSigner::new(&peer_session, TEST_NETWORK_ID),
            transport.dht.did,
            transport.dht.did,
        )?
        .to_wire()?;
    }

    reset_on_message_recursion_depth_for_test();
    let error = callback
        .on_admitted_message_for_test(&peer.to_string(), &current)
        .await
        .expect_err("nested chunks must be rejected");

    assert!(error.to_string().contains("Nested chunk"));
    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    assert_eq!(max_on_message_recursion_depth_for_test(), 1);
    assert_eq!(
        measure
            .snapshot_counters()?
            .into_iter()
            .filter(|(did, counter)| {
                *did == peer && *counter == MeasurementEvent::FailedToReceive
            })
            .count(),
        1
    );
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    assert_eq!(transport.reassembly_budget().buffered_cost_for_test(), 0);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_missing_peer_error_precedes_outbound_capacity_admission() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = Did::from(700_u32);
    let mut permits = Vec::with_capacity(OUTBOUND_DATA_TRANSFER_CAPACITY);
    for _ in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        permits.push(
            transport
                .outbound_schedulers
                .reserve(peer, TransferDemand::new(MessageCategory::Application, 1))
                .await?,
        );
    }
    let payload = MessagePayload::new_send(
        Message::custom(b"missing-peer")?,
        transport.message_signer(),
        peer,
        peer,
    )?;

    let error = transport
        .send_payload_detached_with_outcome(payload)
        .await
        .expect_err("missing route must be checked before capacity");

    assert!(matches!(error, Error::SwarmMissDidInTable(did) if did == peer));
    assert_eq!(
        measure.snapshot_measurements()?,
        vec![(peer, MeasurementEvent::FailedToSend)],
        "a failed local send to an explicit DID is attributable without claiming remote evidence"
    );
    drop(permits);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[traced_test]
#[tokio::test]
async fn test_invalid_inbound_log_omits_transaction_data() -> Result<()> {
    const PRIVATE_MARKER: &str = "PRIVATE-INBOUND-PAYLOAD-7f34c91a";
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let mut payload = MessagePayload::new_send(
        Message::custom(PRIVATE_MARKER.as_bytes())?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?;
    payload.transaction.data.push(171);
    let expected_tx_id = payload.transaction.tx_id;
    let expected_destination = payload.transaction.destination;
    let expected_data_bytes = payload.transaction.data.len();
    let bytes = payload.to_wire()?;
    let expected_wire_bytes = bytes.len();

    callback
        .on_admitted_message_for_test(&peer.to_string(), &bytes)
        .await
        .expect_err("mutating signed transaction data must fail verification");

    let expected_fields = [
        ("peer", format!("Some({peer:?})")),
        ("destination", expected_destination.to_string()),
        ("message_kind", "\"CustomMessage\"".to_owned()),
        ("data_bytes", expected_data_bytes.to_string()),
        ("wire_bytes", expected_wire_bytes.to_string()),
    ];
    let expected_tx_id = expected_tx_id.to_string();
    let marker_debug = crate::tests::byte_debug_fragment(PRIVATE_MARKER.as_bytes());
    logs_assert(|lines: &[&str]| {
        crate::tests::assert_single_structured_log_event(
            lines,
            "rings_core::swarm::callback",
            "inbound message verification failed or expired",
            ("tx_id", &expected_tx_id),
            &expected_fields,
            &[
                PRIVATE_MARKER,
                &marker_debug,
                "transaction.data",
                "MessagePayload {",
            ],
        )
    });
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_pending_disconnected_before_data_channel_open_is_not_reported() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let pending_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);

    pending_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Disconnected)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 1);
    assert_eq!(app_callback.events()?, Vec::new());
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&peer)?);

    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    pending_callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_active_connection_attempt(attempt));
    let events = app_callback.events()?;
    assert_eq!(events, vec![
        WebrtcConnectionState::Connecting,
        WebrtcConnectionState::Connected,
    ]);
    assert!(transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_terminal_event_during_pending_admission_prevents_late_dht_join() -> Result<()> {
    let measure = Arc::new(BlockingConnectMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    app_callback.clear_events()?;

    let opening_transport = Arc::clone(&transport);
    let opening_callback = app_callback.clone();
    let opening = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(opening_transport, opening_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    measure.wait_for_connect_started().await;
    assert!(transport.is_active_connection_attempt(attempt));

    let terminal_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    terminal_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    measure.release_connect();
    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(!transport.is_active_connection_attempt(attempt));
    assert!(!transport.dht.successors().contains(&peer)?);
    let events = app_callback.events()?;
    assert!(events.contains(&WebrtcConnectionState::Closed));
    assert!(!events.contains(&WebrtcConnectionState::Connected));
    Ok(())
}
