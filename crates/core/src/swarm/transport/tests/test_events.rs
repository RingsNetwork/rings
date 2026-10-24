use super::*;
use crate::dht::RelayCustody;
use crate::message::DhtProtocolMode;
#[cfg(feature = "dummy")]
use crate::swarm::application::SwarmEvent;

/// Drive `pending`'s current generation to the terminal `Closed` state.
#[cfg(feature = "dummy")]
async fn close(pending: &PendingPeer) -> Result<()> {
    pending
        .callback
        .on_peer_connection_state_change(&pending.peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))
}

/// Law (per-peer order): the application reads one peer's facts in decision order across
/// generations, `PeerAdmitted`, `PeerRetired`, `PeerAdmitted`: a re-admission is never read
/// ahead of the retirement of the generation it replaces.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_peer_facts_are_read_in_decision_order_across_generations() -> Result<()> {
    let transport = default_transport()?;
    let recorder = ApplicationRecorder::attach(&transport)?;
    let peer_key = SecretKey::random();
    let first = pending_peer_with_key(&transport, peer_key.clone()).await?;
    let peer = first.peer;
    first.admit(&transport).await?;
    assert_eq!(recorder.events(), vec![SwarmEvent::PeerAdmitted { peer }]);

    close(&first).await?;
    let second = pending_peer_with_key(&transport, peer_key).await?;
    second.admit(&transport).await?;

    assert_eq!(recorder.events(), vec![
        SwarmEvent::PeerAdmitted { peer },
        SwarmEvent::PeerRetired { peer },
        SwarmEvent::PeerAdmitted { peer },
    ]);
    assert!(transport.has_active_connection(peer));
    transport.disconnect(peer).await?;
    Ok(())
}

/// Law (no departure without an arrival): an admission retired before the application read it
/// is withdrawn together with its retirement, so the application reads neither, and the
/// transport no longer holds the peer.
#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_admission_retired_before_it_is_read_is_withdrawn() -> Result<()> {
    let transport = default_transport()?;
    let recorder = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;

    close(&pending).await?;

    assert_eq!(recorder.events(), Vec::new());
    assert!(!transport.has_active_connection(pending.peer));
    assert!(!transport.dht.successors().contains(&pending.peer)?);
    Ok(())
}

#[tokio::test]
async fn test_local_did_lifecycle_callbacks_are_ignored() -> Result<()> {
    let transport = default_transport()?;
    let callback = SwarmTransportCallback::new(Arc::clone(&transport));
    let local_cid = transport.dht.did.to_string();

    callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_peer_connection_state_change(&local_cid, WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_data_channel_close(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[tokio::test]
async fn test_mismatched_pending_callback_cancels_attempt_without_admission() -> Result<()> {
    let transport = default_transport()?;
    let peer = SecretKey::random().address().into();
    let callback = SwarmTransportCallback::new(Arc::clone(&transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    let mismatched_callback = SwarmTransportCallback::new(Arc::clone(&transport))
        .with_pending_connection_attempt(attempt);
    let local_cid = transport.dht.did.to_string();

    mismatched_callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(peer).is_none());
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[tokio::test]
async fn test_late_terminal_callback_cannot_remove_replacement_active_slot() -> Result<()> {
    let transport = default_transport()?;
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.cancel_pending_connection(old_attempt).await?);
    let current_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(current_attempt)?);
    let late_callback = SwarmTransportCallback::new(Arc::clone(&transport))
        .with_pending_connection_attempt(old_attempt);

    late_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_active_connection_attempt(current_attempt));
    assert_eq!(transport.admitted_connection_ids(), vec![peer]);
    Ok(())
}

#[test]
fn test_connection_offer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = TransportSpec::default().build()?;
    let matching = ConnectNodeSend {
        sdp: String::new(),
        dht_protocol_mode: DhtProtocolMode::new(0, 1, 0),
        relay_custody: RelayCustody::Holds,
    };
    let mismatched_redundancy = ConnectNodeSend {
        dht_protocol_mode: DhtProtocolMode::new(0, 2, 0),
        ..matching.clone()
    };

    assert!(transport.accepts_connection_offer(&matching));
    assert!(!transport.accepts_connection_offer(&mismatched_redundancy));
    Ok(())
}

#[test]
fn test_connection_answer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = TransportSpec::default().build()?;
    let matching = ConnectNodeReport {
        sdp: String::new(),
        dht_protocol_mode: DhtProtocolMode::new(0, 1, 0),
        relay_custody: RelayCustody::Holds,
    };
    let mismatched_redundancy = ConnectNodeReport {
        dht_protocol_mode: DhtProtocolMode::new(0, 2, 0),
        ..matching.clone()
    };

    assert!(transport.accepts_connection_answer(&matching));
    assert!(!transport.accepts_connection_answer(&mismatched_redundancy));
    Ok(())
}

#[tokio::test]
async fn test_disconnected_observation_is_once_per_connection_epoch() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = transport_with_measure(measure.clone())?;
    let peer: Did = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    transport.record_peer_disconnected(attempt).await;
    transport.record_peer_disconnected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());
    transport.record_peer_connected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_none());
    transport.record_peer_disconnected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());

    assert_eq!(measure.snapshot_counters()?.as_slice(), &[
        (peer, MeasurementEvent::Disconnected),
        (peer, MeasurementEvent::Connected),
        (peer, MeasurementEvent::Disconnected),
    ]);

    Ok(())
}

#[tokio::test]
async fn test_dht_candidate_order_uses_peer_quality_without_dropping_candidates() -> Result<()> {
    let degraded = SecretKey::random().address().into();
    let unknown = SecretKey::random().address().into();
    let healthy = SecretKey::random().address().into();
    let measure = Arc::new(RecordingMeasure::default());
    measure.set_quality(degraded, PeerQuality::Degraded)?;
    measure.set_quality(healthy, PeerQuality::Healthy)?;
    let transport = transport_with_measure(measure)?;

    let ordered = transport
        .order_dht_candidates_by_quality([degraded, unknown, healthy])
        .await;

    assert_eq!(ordered, vec![healthy, unknown, degraded]);

    Ok(())
}

#[tokio::test]
async fn test_malformed_outbound_payload_is_rejected_before_connection_admission() -> Result<()> {
    let transport = TransportSpec::default().build()?;
    let peer = SecretKey::random().address().into();
    let mut payload = MessagePayload::new_send(
        Message::custom(b"malformed outbound payload")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    payload.transaction.data = vec![0xff];

    assert!(matches!(
        transport.application_egress().send_payload(payload).await,
        Err(Error::CodecDeserialize(_))
    ));
    Ok(())
}
