use super::*;
use crate::dht::LiveDid;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
async fn admitted_dummy_transport() -> Result<(Arc<SwarmTransport>, Did)> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    Ok((transport, peer))
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_disconnected_open_transport_cannot_commit_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Disconnected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;

    assert!(transport.begin_connection_admission_for_test(attempt)?);
    assert!(matches!(
        transport.commit_connection_admission(attempt),
        Err(Error::TransportNotReady {
            state: WebrtcConnectionState::Disconnected,
            data_channel_open: true,
        })
    ));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);

    assert!(transport.cancel_pending_connection(attempt).await?);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_data_channel_open_before_peer_state_converges_preserves_pending_admission(
) -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;

    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.unadmitted_attempt(peer)?, Some(attempt));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.successors().contains(&peer)?);
    assert!(transport.get_raw_connection(peer).is_some());

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connected)?;
    callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Connected)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection_attempt(attempt));
    assert!(transport.dht.successors().contains(&peer)?);
    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_non_ready_transport_is_admitted_but_not_routable_or_live() -> Result<()> {
    let (transport, peer) = admitted_dummy_transport().await?;
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Disconnected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;

    assert!(transport.is_admitted_connection(peer));
    assert!(transport.get_connection(peer).is_none());
    assert!(!PayloadSender::is_connected(transport.as_ref(), peer));
    let raw = transport
        .get_raw_connection(peer)
        .ok_or(Error::SwarmMissTransport(peer))?;
    assert!(!raw.live().await);

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(false))?;
    assert!(transport.get_connection(peer).is_none());
    assert!(!PayloadSender::is_connected(transport.as_ref(), peer));
    assert!(!raw.live().await);

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_tracked_send_rejects_disconnected_open_transport() -> Result<()> {
    let (transport, peer) = admitted_dummy_transport().await?;
    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Disconnected)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;

    let payload = MessagePayload::new_send(
        Message::custom(b"must-not-send")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    assert!(matches!(
        transport.send_payload_tracked(payload).await,
        Err(Error::TransportNotReady {
            state: WebrtcConnectionState::Disconnected,
            data_channel_open: true,
        })
    ));

    transport.disconnect(peer).await?;
    Ok(())
}
