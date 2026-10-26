use super::*;

/// A frame admitted by core while its generation was live is a verified
/// receive; if the generation is retired before the actor dispatches it, the
/// application never sees it.
#[tokio::test]
async fn test_retired_frame_queued_on_lane_is_not_delivered() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.activate(&transport)?;
    let (peer, attempt) = (pending.peer, pending.attempt()?);
    let admission_turn = pending
        .callback
        .inbound_for_test()
        .hold_application_admission_for_test()?;
    let frame = pending.custom_message_wire(&transport, b"retired-before-lane-ticket-release")?;
    let delivery = spawn_inbound_delivery(Arc::clone(&pending.callback), peer.to_string(), frame);

    // Core admission completes while the generation is live: the frame is
    // verified, counted as received, and queued behind the held lane. The
    // delivery itself completes only when the actor dispatches the frame.
    measure
        .await_recorded(|measure| {
            successful_receive_count(measure, peer).is_ok_and(|count| count == 1)
        })
        .await;
    assert_eq!(
        pending
            .callback
            .inbound_for_test()
            .admitted_count_for_test(),
        1
    );
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
    pending
        .callback
        .inbound_for_test()
        .await_admitted_count_for_test(|admitted| admitted == 0)
        .await;
    assert_eq!(successful_receive_count(&measure, peer)?, 1);
    assert_eq!(application.observed(), 0);
    assert_eq!(application.inbounds(), 0);
    Ok(())
}

/// A chunked control message keeps its place among later control (load-bounds paper, Prop.
/// "Order per category", #927). The frames held before admission are released at once,
/// unpaced: the chunks of a control message, on the control carrier as a small advertised
/// message size makes them (#1130), then a later control frame. The later frame waits for the
/// reassembled message, where the old control exemption let it run first: each chunk's
/// reassembly takes its deterministic service time, during which the later frame is ready.
#[tokio::test(start_paused = true)]
async fn test_a_released_chunked_control_message_stays_ahead_of_later_control() -> Result<()> {
    let runtime = crate::simulation::SimulationRuntimeGuard::enter(
        927,
        1_700_000_000_000,
        crate::simulation::ProtectionProfile::ALL_ENABLED,
    )
    .map_err(|error| Error::InvalidMessage(format!("{error:?}")))?;
    runtime
        .enable_reassembly_service()
        .map_err(|error| Error::InvalidMessage(format!("{error:?}")))?;
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    let (cid, local) = (pending.peer.to_string(), transport.dht.did);
    let control_carrier = Some(MessageCategory::DhtControl.channel_lane());
    let (first, later) = (Did::from(1_u32), Did::from(2_u32));

    let chunked = local_wire(noop_control_message(first), &pending.session, local)?;
    let chunks = Chunk::stream(chunked, 64).collect::<Vec<_>>();
    assert!(chunks.len() > 1, "the control message spans several chunks");
    let later_frame = local_wire(noop_control_message(later), &pending.session, local)?;
    let frames = chunks
        .into_iter()
        .map(|chunk| local_wire(Message::Chunk(chunk), &pending.session, local))
        .chain([Ok(later_frame)])
        .collect::<Result<Vec<_>>>()?;
    let held = frames.len();
    for frame in frames {
        pending
            .callback
            .on_carried_message_for_test(&cid, &frame, control_carrier)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), held);

    pending.admit(&transport).await?;
    let mut reports = Vec::new();
    while reports.len() < 2 {
        if let Message::FindSuccessorReport(report) = application.next_tapped().await? {
            reports.push(report.did);
        }
    }
    assert_eq!(reports, vec![first, later]);
    Ok(())
}
