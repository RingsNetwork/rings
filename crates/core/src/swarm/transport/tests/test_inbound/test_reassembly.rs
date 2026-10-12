//! Reassembled messages through the inbound path: one logical receive each, order kept
//! without blocking control, and the lane shape verified before the lane transition.

use super::deliver_chunked;
use super::deliver_leading_chunks;
use super::hold_saturated_application_capacity;
use super::noop_control_message;
use super::spawn_inbound_delivery;
use crate::chunk::Chunk;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasurementEvent;
use crate::message::FoundEntry;
use crate::message::Message;
use crate::swarm::callback::InboundLane;
use crate::swarm::transport::tests::default_transport;
use crate::swarm::transport::tests::failed_receive_count;
use crate::swarm::transport::tests::local_payload;
use crate::swarm::transport::tests::local_wire;
use crate::swarm::transport::tests::pending_peer;
use crate::swarm::transport::tests::successful_receive_count;
use crate::swarm::transport::tests::transport_with_measure;
use crate::swarm::transport::tests::unbound_peer;
use crate::swarm::transport::tests::ApplicationRecorder;
use crate::swarm::transport::tests::RecordingMeasure;
use crate::swarm::transport::DelegateeKey;
use crate::swarm::Arc;
use crate::swarm::Did;
use crate::swarm::SwarmTransportCallback;
use crate::tests::TEST_NETWORK_ID;

#[tokio::test]
async fn test_chunk_reassembly_records_one_exact_logical_receive() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let peer = pending.peer;
    let logical_payload = pending.custom_payload(&transport, &[41; 512])?;
    let expected_useful_bytes = u64::try_from(logical_payload.transaction.data.len())
        .map_err(|_| Error::MessageSizeOverflow)?;

    let (last, _chunks) = deliver_leading_chunks(
        &pending.callback,
        &peer.to_string(),
        &pending.session,
        transport.dht.did,
        logical_payload.to_wire()?,
    )
    .await?;
    pending.receive(&last).await?;

    let received = measure
        .snapshot_measurements()?
        .into_iter()
        .filter_map(|(observed_peer, event)| match event {
            MeasurementEvent::Received { useful_bytes } if observed_peer == peer => {
                Some(useful_bytes)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(received, vec![expected_useful_bytes]);
    assert_eq!(failed_receive_count(&measure, peer)?, 0);
    assert_eq!(application.inbounds(), 1);
    Ok(())
}

#[tokio::test]
async fn test_reassembled_undecodable_message_records_one_failure_only() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let undecodable = local_payload(vec![0xff_u8; 512], &pending.session, transport.dht.did)?;

    let (last, _chunks) = deliver_leading_chunks(
        &pending.callback,
        &pending.peer.to_string(),
        &pending.session,
        transport.dht.did,
        undecodable.to_wire()?,
    )
    .await?;
    assert!(pending.receive(&last).await.is_err());

    assert_eq!(failed_receive_count(&measure, pending.peer)?, 1);
    assert_eq!(successful_receive_count(&measure, pending.peer)?, 0);
    assert_eq!(application.inbounds(), 0);
    Ok(())
}

/// A reassembled message is handed to its logical lane in arrival order: while the hand-off
/// cannot start, because an earlier application delivery occupies the held application lane, a
/// later storage frame waits behind it and a control frame does not.
#[tokio::test]
async fn test_reassembly_handoff_preserves_data_order_without_blocking_control() -> Result<()> {
    let transport = default_transport()?;
    let (application, lane_hold) =
        ApplicationRecorder::attach_holding_application_lane(&transport)?;
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let local = transport.dht.did;
    let cid = peer.peer.to_string();

    let blocker = peer.custom_message_wire(&transport, b"holds-the-application-lane")?;
    let blocker_delivery = spawn_inbound_delivery(Arc::clone(&peer.callback), cid.clone(), blocker);
    assert_eq!(
        application.next_tapped_custom().await?,
        b"holds-the-application-lane".to_vec()
    );

    let (final_delivery, chunks) = deliver_chunked(
        &peer.callback,
        &cid,
        &peer.session,
        local,
        Message::custom(b"reassembled-first")?,
    )
    .await?;
    for _ in 0..chunks {
        assert!(matches!(
            application.next_tapped().await?,
            Message::Chunk(_)
        ));
    }

    let later = local_wire(
        Message::FoundEntry(FoundEntry {
            data: Vec::new(),
            misses: Vec::new(),
            resource: Did::from(91_u32),
            redundancy: 1,
        }),
        &peer.session,
        local,
    )?;
    let later_delivery = spawn_inbound_delivery(Arc::clone(&peer.callback), cid.clone(), later);

    let control = local_wire(noop_control_message(local), &peer.session, local)?;
    spawn_inbound_delivery(Arc::clone(&peer.callback), cid, control)
        .await
        .map_err(|_| Error::InvalidMessage("control lane task panicked".to_string()))??;
    // Only the control frame entered the logical stage behind the held hand-off.
    assert!(matches!(
        application.next_tapped().await?,
        Message::FindSuccessorReport(_)
    ));
    assert!(application.drain_tapped()?.is_empty());

    drop(lane_hold);
    blocker_delivery
        .await
        .map_err(|_| Error::InvalidMessage("blocker task panicked".to_string()))??;
    final_delivery
        .await
        .map_err(|_| Error::InvalidMessage("final chunk task panicked".to_string()))??;
    let later_result = later_delivery
        .await
        .map_err(|_| Error::InvalidMessage("later storage task panicked".to_string()))?;
    assert!(
        later_result.is_err(),
        "unsolicited storage response must fail"
    );
    assert_eq!(
        application.next_tapped_custom().await?,
        b"reassembled-first".to_vec()
    );
    assert!(matches!(
        application.next_tapped().await?,
        Message::FoundEntry(_)
    ));
    application.wait_for_inbounds_at_least(2).await;
    assert_eq!(application.inbound_custom_data(), vec![
        b"holds-the-application-lane".to_vec(),
        b"reassembled-first".to_vec(),
    ]);
    Ok(())
}

#[tokio::test]
async fn test_transport_preparation_authenticates_every_reserved_lane() -> Result<()> {
    let transport = default_transport()?;
    let peer_key = SecretKey::random();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let control = local_wire(
        noop_control_message(transport.dht.did),
        &peer_session,
        transport.dht.did,
    )?;
    let application = local_wire(
        Message::custom(b"application")?,
        &peer_session,
        transport.dht.did,
    )?;
    let storage = local_wire(
        Message::FoundEntry(FoundEntry {
            data: Vec::new(),
            misses: Vec::new(),
            resource: Did::from(7_u32),
            redundancy: 1,
        }),
        &peer_session,
        transport.dht.did,
    )?;
    let chunks: Vec<Chunk> = Chunk::stream(application.clone(), 32).collect();
    let chunk = chunks
        .into_iter()
        .next()
        .ok_or(Error::InboundActorInvariantViolation)?;
    let reassembly = local_wire(Message::Chunk(chunk), &peer_session, transport.dht.did)?;

    let control_lane =
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &control)?;
    assert_eq!(control_lane, InboundLane::DhtControl);
    let truncated = control
        .get(..control.len().saturating_sub(1))
        .ok_or(Error::InboundActorInvariantViolation)?;
    assert!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, truncated)
            .is_err(),
        "a truncated control-shaped payload must not claim control capacity"
    );
    for (name, frame) in [
        ("control", &control),
        ("application", &application),
        ("storage", &storage),
        ("reassembly", &reassembly),
    ] {
        let mut damaged = frame.to_vec();
        let final_byte = damaged
            .last_mut()
            .ok_or(Error::InboundActorInvariantViolation)?;
        *final_byte ^= 1;
        assert!(
            crate::swarm::callback::prepare_transport_frame_lane_for_test(
                TEST_NETWORK_ID,
                &damaged
            )
            .is_err(),
            "unauthenticated {name} shape must not claim reserved capacity"
        );
    }
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(
            TEST_NETWORK_ID,
            &application
        )?,
        InboundLane::Application
    );
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &storage)?,
        InboundLane::Storage
    );
    assert_eq!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(
            TEST_NETWORK_ID,
            &reassembly
        )?,
        InboundLane::Reassembly
    );
    assert!(
        crate::swarm::callback::prepare_transport_frame_lane_for_test(TEST_NETWORK_ID, &[0xff])
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn test_reassembled_control_shape_is_verified_before_lane_transition() -> Result<()> {
    let transport = default_transport()?;
    let saturated = Arc::new(SwarmTransportCallback::new(Arc::clone(&transport)));
    let capacity_holds = hold_saturated_application_capacity(&saturated)?;

    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let mut tampered = local_payload(
        noop_control_message(transport.dht.did),
        &peer.session,
        transport.dht.did,
    )?;
    tampered.transaction.data.push(0);
    assert_eq!(
        crate::message::MessageKind::from_wire(&tampered.transaction.data)?.class(),
        crate::message::MessageCategory::DhtControl
    );
    let (final_frame, _chunks) = deliver_leading_chunks(
        &peer.callback,
        &peer.peer.to_string(),
        &peer.session,
        transport.dht.did,
        tampered.to_wire()?,
    )
    .await?;
    let error = peer
        .callback
        .on_admitted_message_for_test(&peer.peer.to_string(), &final_frame)
        .await
        .expect_err("tampered reassembled control frame must fail verification");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::InvalidMessage(message))
            if message == "message verification failed or message expired"
    ));

    drop(capacity_holds);
    Ok(())
}
