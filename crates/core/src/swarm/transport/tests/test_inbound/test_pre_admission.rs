use super::*;

/// A frame admitted by core while its generation was live is a verified
/// receive; if the generation is retired before the actor dispatches it, the
/// application never sees it.
#[tokio::test]
async fn test_retired_frame_queued_on_lane_is_not_delivered() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let application = ApplicationRecorder::attach(&transport)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport)).with_pending_connection_attempt(attempt),
    );
    let admission_turn = callback.hold_application_admission_for_test()?;
    let frame = local_wire(
        Message::custom(b"retired-before-lane-ticket-release")?,
        &peer_session,
        transport.dht.did,
    )?;
    let delivery = spawn_inbound_delivery(Arc::clone(&callback), peer.to_string(), frame);

    // Core admission completes while the generation is live: the frame is
    // verified, counted as received, and queued behind the held lane. The
    // delivery itself completes only when the actor dispatches the frame.
    measure
        .await_recorded(|measure| {
            successful_receive_count(measure, peer).is_ok_and(|count| count == 1)
        })
        .await;
    assert_eq!(callback.inbound_admitted_count_for_test(), 1);
    assert_eq!(
        transport.retire_active_connection_for_test(attempt, |_| Ok(()))?,
        Some(())
    );
    drop(admission_turn);

    // The released lane dispatches the frame to a retired generation: the
    // actor drops it, completes the delivery, and releases the permit.
    delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))??;
    callback
        .await_inbound_admitted_count_for_test(|admitted| admitted == 0)
        .await;
    assert_eq!(successful_receive_count(&measure, peer)?, 1);
    assert_eq!(application.validates(), 0);
    assert_eq!(application.inbounds(), 0);
    Ok(())
}
