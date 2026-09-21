//! The link stage of one inbound connection: frames whose session slots are references, the
//! hold behind a miss, and the link-control frames that repair it. Every wait is on the
//! application callback's delivery event.

use super::*;
use crate::message::HopBudget;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::PerSlot;
use crate::message::SessionRef;
use crate::message::Transaction;
use crate::message::WirePayload;
use crate::session::SessionDigest;
use crate::swarm::callback::SESSION_HOLD_CAPACITY;
use crate::swarm::transport::dispatched_link_control_for_test;
use crate::swarm::transport::LINK_CONTROL_IN_FLIGHT_CAPACITY;
use crate::tests::default::dummy_hooks::PausedDispatchGuard;
use crate::tests::session_sk_with_ttl;

/// The typed refusal of a referenced frame outside a link, from the callback's boxed error.
fn unresolved_reference(refusal: &(dyn std::error::Error + 'static)) -> Option<SessionDigest> {
    match refusal.downcast_ref::<Error>() {
        Some(Error::SessionReferenceUnresolved(digest)) => Some(*digest),
        _ => None,
    }
}

/// The frame bytes of `payload` with both session slots sent by reference.
fn referenced_wire(payload: &MessagePayload) -> Result<Vec<u8>> {
    let sessions = payload.sessions();
    let references = PerSlot {
        origin: SessionRef::Digest(sessions.origin.digest()?),
        hop: SessionRef::Digest(sessions.hop.digest()?),
    };
    WirePayload::view(payload, references)
        .to_wire()
        .map(|wire| wire.to_vec())
}

/// The frame bytes of `payload` with the origin session inline and the hop session by
/// reference: a slot mix only a link can carry.
fn hop_referenced_wire(payload: &MessagePayload) -> Result<Vec<u8>> {
    let sessions = payload.sessions();
    let references = PerSlot {
        origin: SessionRef::inline(sessions.origin),
        hop: SessionRef::Digest(sessions.hop.digest()?),
    };
    WirePayload::view(payload, references)
        .to_wire()
        .map(|wire| wire.to_vec())
}

/// A custom message from a stranger, carried one hop by `pending`'s peer to the local node: a
/// session this connection learns only if told.
fn stranger_payload(
    pending: &PendingPeer,
    transport: &SwarmTransport,
    stranger: &SessionSk,
    data: &[u8],
) -> Result<MessagePayload> {
    let transaction = Transaction::new(
        transport.dht.did,
        crate::utils::new_uuid(),
        0,
        Message::custom(data)?,
        MessageSigner::new(stranger, TEST_NETWORK_ID),
    )?;
    let relay = MessageRelay::new(transport.dht.did, transport.dht.did, HopBudget::MAX);
    MessagePayload::new(
        transaction,
        MessageSigner::new(&pending.session, TEST_NETWORK_ID),
        relay,
    )
}

/// A custom message from `pending`'s peer to the local node.
fn custom_payload(
    pending: &PendingPeer,
    transport: &SwarmTransport,
    data: &[u8],
) -> Result<MessagePayload> {
    MessagePayload::new_send(
        Message::custom(data)?,
        MessageSigner::new(&pending.session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )
}

/// Steady state: once a verified frame carried a session inline, later frames that reference
/// it are resolved, verified, and delivered like any other.
#[tokio::test]
async fn test_referenced_session_resolves_after_a_verified_inline_frame() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    pending
        .receive(&pending.custom_message_wire(&transport, b"inline")?)
        .await?;
    let steady = custom_payload(&pending, &transport, b"referenced")?;
    pending.receive(&referenced_wire(&steady)?).await?;
    app_callback.wait_for_inbounds_at_least(2).await;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"inline".to_vec(),
        b"referenced".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Miss, then announcement: a frame that references a session this connection never carried is
/// held, not failed; a frame that resolves passes it; the peer's announcement releases it.
#[tokio::test]
async fn test_missed_session_holds_frames_until_the_peer_announces_it() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let missed = stranger_payload(&pending, &transport, &stranger, b"missed")?;
    pending.receive(&referenced_wire(&missed)?).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"passing")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    assert!(!measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));

    let announcement = LinkControl::Announce(stranger.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(2).await;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"passing".to_vec(),
        b"missed".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Miss on the hop slot alone is held and repaired the same way, and the released frame's
/// inline slot is confirmed to the peer as an arriving frame's would be: the question it
/// raised is asked once, on arrival, and the confirmation is emitted on release.
#[tokio::test]
async fn test_missed_hop_session_is_repaired_by_announcement() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    let digest = pending.session.session().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    let missed = custom_payload(&pending, &transport, b"hop-missed")?;
    pending.receive(&hop_referenced_wire(&missed)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![(pending.peer, LinkControl::Request(digest))]
    );

    let announcement = LinkControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(1).await;

    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"hop-missed".to_vec()
    ]);
    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![
            (pending.peer, LinkControl::Request(digest)),
            (pending.peer, LinkControl::Known(digest)),
        ]
    );
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bound, charging): a frame that finds the hold full is dropped as a loss at this end's
/// capacity, not charged to the peer, and the oldest held frame's question is asked again;
/// what is held stays held.
#[tokio::test]
async fn test_hold_overflow_drops_the_newcomer_uncharged_and_asks_the_oldest_question_again(
) -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    // The peer's own session is taught first, so each held frame misses its origin alone.
    pending
        .receive(&pending.custom_message_wire(&transport, b"teach-the-hop")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    let dispatched_before = dispatched_link_control_for_test().len();

    let oldest_stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let oldest_digest = oldest_stranger.session().digest()?;
    let oldest = stranger_payload(&pending, &transport, &oldest_stranger, b"oldest")?;
    pending.receive(&referenced_wire(&oldest)?).await?;
    for held in 1..SESSION_HOLD_CAPACITY {
        let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
        let data = format!("held-{held}");
        let missed = stranger_payload(&pending, &transport, &stranger, data.as_bytes())?;
        pending.receive(&referenced_wire(&missed)?).await?;
    }
    assert_eq!(
        pending.callback.session_hold_count_for_test(),
        SESSION_HOLD_CAPACITY
    );
    assert!(!measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));

    let newcomer_stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let newcomer = stranger_payload(&pending, &transport, &newcomer_stranger, b"newcomer")?;
    pending.receive(&referenced_wire(&newcomer)?).await?;

    assert_eq!(
        pending.callback.session_hold_count_for_test(),
        SESSION_HOLD_CAPACITY
    );
    assert!(!measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));
    assert_eq!(
        dispatched_link_control_for_test().last(),
        Some(&(pending.peer, LinkControl::Request(oldest_digest)))
    );
    assert!(!dispatched_link_control_for_test()
        .split_off(dispatched_before)
        .contains(&(
            pending.peer,
            LinkControl::Request(newcomer_stranger.session().digest()?)
        )));
    assert_eq!(app_callback.inbounds(), 1);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (scope): the receiver's table lives in the callback of one connection generation. A
/// session the peer's previous generation carried inline is unknown to the next generation of
/// the same peer: a reference to it there is a miss, held and asked about, never resolved
/// from the old generation's table.
#[tokio::test]
async fn test_receiver_table_does_not_outlive_the_connection_generation() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let peer_key = SecretKey::random();
    let first = pending_peer_with_key(&transport, &app_callback, peer_key.clone()).await?;
    first.admit(&transport).await?;
    first
        .receive(&first.custom_message_wire(&transport, b"inline")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    transport.disconnect(first.peer).await?;

    let next = pending_peer_with_key(&transport, &app_callback, peer_key).await?;
    next.admit(&transport).await?;
    let digest = next.session.session().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let referenced = custom_payload(&next, &transport, b"referenced-on-the-next-generation")?;
    next.receive(&referenced_wire(&referenced)?).await?;

    assert_eq!(next.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![(next.peer, LinkControl::Request(digest))]
    );
    transport.disconnect(next.peer).await?;
    Ok(())
}

/// A peer that disclaims the session it referenced fails the frames that await it, and the
/// failure is charged to that peer; frames that resolve are unaffected.
#[tokio::test]
async fn test_disclaimed_session_fails_awaiting_frames_and_leaves_resolved_ones_unaffected(
) -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let missed = stranger_payload(&pending, &transport, &stranger, b"never-resolved")?;
    let digest = stranger.session().digest()?;
    pending.receive(&referenced_wire(&missed)?).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"passing")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);

    let disclaimer = LinkControl::Unknown(digest).to_wire()?;
    pending.receive(disclaimer.as_ref()).await?;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 1);
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"passing".to_vec()
    ]);
    assert!(measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Nothing unsolicited is cached: an announcement nobody awaits does not make a later
/// reference to it resolvable.
#[tokio::test]
async fn test_unsolicited_announcement_does_not_populate_the_link() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let announcement = LinkControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    let referenced = custom_payload(&pending, &transport, b"referenced")?;
    pending.receive(&referenced_wire(&referenced)?).await?;

    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bound in time): a frame held for a session the peer never supplies is dropped by the
/// inbound actor's periodic sweep once it waited past the hold timeout, charged to the peer as
/// a receive failure, and its transport lease goes with it. The sweep reads the injected clock,
/// so advancing it past the timeout is the whole event; the wait is for two cleanup passes,
/// since the one in progress may have read the clock before it advanced, and the one after it
/// began after it did.
#[tokio::test]
async fn test_frames_held_past_the_timeout_are_swept_and_charged() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let now_ms = Arc::new(Mutex::new(crate::utils::get_epoch_ms()));
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer_with(
        &transport,
        &app_callback,
        SecretKey::random(),
        Some(Arc::clone(&now_ms)),
    )
    .await?;
    pending.admit(&transport).await?;
    let peer = pending.peer;

    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let missed = stranger_payload(&pending, &transport, &stranger, b"never-answered")?;
    pending.receive(&referenced_wire(&missed)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    let passes_before = pending.callback.reassembly_cleanup_passes_for_test();

    *now_ms.lock().map_err(|_| Error::LockPoisoned)? +=
        crate::swarm::transport::SESSION_HOLD_TIMEOUT.as_millis() + 1;
    pending
        .callback
        .await_reassembly_cleanup_passes_for_test(|passes| passes >= passes_before + 2)
        .await;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert!(measure
        .snapshot_counters()?
        .contains(&(peer, MeasureCounter::FailedToReceive)));
    assert_eq!(app_callback.inbounds(), 0);
    transport.disconnect(peer).await?;
    Ok(())
}

/// Law (generation): a control frame judged on a connection generation that is no longer
/// current is refused, never sent on the newer generation of the same peer, whose tables never
/// saw what it would confirm.
#[tokio::test]
async fn test_control_frames_judged_on_a_superseded_generation_are_not_sent() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let peer_key = SecretKey::random();
    let first = pending_peer_with_key(&transport, &app_callback, peer_key.clone()).await?;
    first.admit(&transport).await?;
    transport.disconnect(first.peer).await?;
    let next = pending_peer_with_key(&transport, &app_callback, peer_key).await?;
    next.admit(&transport).await?;
    let dispatched_before = dispatched_link_control_for_test().len();
    let inbounds_before = app_callback.inbounds();

    // The old generation's callback still verifies the frame (nothing is charged) and would
    // confirm its session; the gate then drops the frame as superseded, so it is not delivered.
    let late = first.custom_message_wire(&transport, b"late-on-the-old-generation")?;
    first.receive(&late).await?;

    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);
    assert!(!measure
        .snapshot_counters()?
        .contains(&(first.peer, MeasureCounter::FailedToReceive)));
    assert_eq!(first.callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), inbounds_before);
    transport.disconnect(next.peer).await?;
    Ok(())
}

/// Law (detachment): the read loop is never paced by a control-frame send. With the dummy
/// transport's dispatch gate closed, so that the confirmation's send cannot complete, an inline
/// frame still completes its delivery; a send awaited in the read loop would sit behind the
/// gate instead, and the frame with it. The gate is then opened and the send goes through.
#[tokio::test]
async fn test_control_frames_never_pace_the_read_loop() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    let digest = pending.session.session().digest()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    let dispatch_gate = PausedDispatchGuard::new();
    pending
        .receive(&pending.custom_message_wire(&transport, b"delivered-anyway")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![(pending.peer, LinkControl::Known(digest))]
    );
    drop(dispatch_gate);

    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bound): at most `LINK_CONTROL_IN_FLIGHT_CAPACITY` control sends are in flight to one
/// peer. With the peer's whole budget held by sends in flight, an inline frame's confirmation
/// is refused, not dispatched, while the frame itself is still delivered; once a send ends,
/// the next frame that teaches the session confirms it.
#[tokio::test]
async fn test_control_sends_beyond_the_in_flight_budget_are_refused() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    let digest = pending.session.session().digest()?;
    transport.outbound_schedulers.handle(pending.peer)?;
    let in_flight = (0..LINK_CONTROL_IN_FLIGHT_CAPACITY)
        .map(|_| {
            transport
                .outbound_schedulers
                .link_control_permit(pending.peer)
                .flatten()
                .ok_or(Error::LinkControlInFlightCapacity(pending.peer))
        })
        .collect::<Result<Vec<_>>>()?;
    let dispatched_before = dispatched_link_control_for_test().len();

    pending
        .receive(&pending.custom_message_wire(&transport, b"beyond-the-budget")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(1).await;
    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);

    drop(in_flight);
    pending
        .receive(&pending.custom_message_wire(&transport, b"within-the-budget-again")?)
        .await?;
    app_callback.wait_for_inbounds_at_least(2).await;
    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![(pending.peer, LinkControl::Known(digest))]
    );
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// A question from the peer is answered from what this end announced on the link: the session
/// itself when this end sent it inline, a disclaimer for a session it never sent.
#[tokio::test]
async fn test_a_request_is_answered_from_the_announced_table() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    let own_session = transport.session_sk.session();
    let own_digest = own_session.digest()?;
    // This end announces its session as the worker would, by encoding one frame to the peer.
    let announced = MessagePayload::new_send(
        Message::custom(b"announces-my-session")?,
        MessageSigner::new(&transport.session_sk, TEST_NETWORK_ID),
        transport.dht.did,
        pending.peer,
    )?;
    let generation = transport
        .active_attempt(pending.peer)?
        .ok_or(Error::SwarmMissDidInTable(pending.peer))?
        .generation();
    let _announcing_frame = transport.outbound_schedulers.encode_for_test(
        pending.peer,
        generation,
        &announced,
        crate::utils::get_epoch_ms(),
    )?;
    let dispatched_before = dispatched_link_control_for_test().len();

    pending
        .receive(LinkControl::Request(own_digest).to_wire()?.as_ref())
        .await?;
    let never_sent = SessionSk::new_with_seckey(&SecretKey::random())?
        .session()
        .digest()?;
    pending
        .receive(LinkControl::Request(never_sent).to_wire()?.as_ref())
        .await?;

    assert_eq!(
        dispatched_link_control_for_test().split_off(dispatched_before),
        vec![
            (pending.peer, LinkControl::Announce(own_session)),
            (pending.peer, LinkControl::Unknown(never_sent)),
        ]
    );
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (link): a callback bound to no handshake is on no link. A referenced frame there is
/// refused as unresolvable, held nowhere, and nothing is asked.
#[tokio::test]
async fn test_referenced_frame_on_an_unbound_callback_is_refused() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    let unbound = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let dispatched_before = dispatched_link_control_for_test().len();

    let referenced = custom_payload(&pending, &transport, b"referenced-off-link")?;
    let refused = unbound
        .on_admitted_message_for_test(&pending.peer.to_string(), &referenced_wire(&referenced)?)
        .await;

    let refusal = refused.expect_err("a reference resolves only on a link");
    assert_eq!(
        unresolved_reference(refusal.as_ref()),
        Some(pending.session.session().digest()?)
    );
    assert_eq!(unbound.session_hold_count_for_test(), 0);
    assert_eq!(dispatched_link_control_for_test().len(), dispatched_before);
    assert_eq!(app_callback.inbounds(), 0);
    Ok(())
}

/// Law (link): a frame from a peer other than the one the callback is bound to is on no link:
/// a reference in it is refused, and a control frame from it changes nothing, so a stranger can
/// neither fill the link's table nor release what waits on it.
#[tokio::test]
async fn test_frames_from_a_peer_other_than_the_bound_one_are_off_the_link() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;
    let stranger_did: Did = SecretKey::random().address().into();

    let referenced = custom_payload(&pending, &transport, b"referenced-by-a-stranger")?;
    let refused = pending
        .callback
        .on_admitted_message_for_test(&stranger_did.to_string(), &referenced_wire(&referenced)?)
        .await;
    let refusal = refused.expect_err("a reference resolves only on the bound link");
    assert_eq!(
        unresolved_reference(refusal.as_ref()),
        Some(pending.session.session().digest()?)
    );
    assert_eq!(pending.callback.session_hold_count_for_test(), 0);

    // A frame the bound peer sends by reference waits; the stranger's announcement of the very
    // session it awaits is not the peer's word and releases nothing.
    let awaited = custom_payload(&pending, &transport, b"awaiting-the-peer")?;
    pending.receive(&referenced_wire(&awaited)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    let announcement = LinkControl::Announce(pending.session.session()).to_wire()?;
    pending
        .callback
        .on_admitted_message_for_test(&stranger_did.to_string(), announcement.as_ref())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (learning): a frame teaches the link before it is gated. Before this end admits the
/// handshake, an inline frame is held for admission and still teaches its session, so a
/// referenced frame that follows resolves, is held for admission too, and both are delivered
/// by admission itself; the session hold never sees either.
#[tokio::test]
async fn test_a_frame_teaches_the_link_before_it_is_gated() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;

    pending
        .receive(&pending.custom_message_wire(&transport, b"inline-before-admission")?)
        .await?;
    let referenced = custom_payload(&pending, &transport, b"referenced-before-admission")?;
    pending.receive(&referenced_wire(&referenced)?).await?;

    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 2);
    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 0);

    pending.admit(&transport).await?;
    app_callback.wait_for_inbounds_at_least(2).await;
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"inline-before-admission".to_vec(),
        b"referenced-before-admission".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (charging, expiry): an announcement of a delegation that no longer verifies is refused,
/// and the frames that awaited it are charged to the peer; the frame is not resurrected by an
/// expired delegation.
#[tokio::test]
async fn test_an_expired_announcement_is_refused_and_charged() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    // Shorter than the hold timeout, so the announcement is judged expired while the frame is
    // not yet sweepable, and long enough to sign under the system clock below. Stamped before
    // the inbound clock is captured, so the clock advanced past the lifetime is past the
    // delegation's end.
    const SHORT_LIFETIME_MS: u64 = 900;
    let short_lived = session_sk_with_ttl(SHORT_LIFETIME_MS)?;
    let now_ms = Arc::new(Mutex::new(crate::utils::get_epoch_ms()));
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer_with(
        &transport,
        &app_callback,
        SecretKey::random(),
        Some(Arc::clone(&now_ms)),
    )
    .await?;
    pending.admit(&transport).await?;

    let missed = stranger_payload(
        &pending,
        &transport,
        &short_lived,
        b"awaits-an-expiring-key",
    )?;
    pending.receive(&referenced_wire(&missed)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);

    *now_ms.lock().map_err(|_| Error::LockPoisoned)? += u128::from(SHORT_LIFETIME_MS) + 1;
    let announcement = LinkControl::Announce(short_lived.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    assert!(measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));
    transport.disconnect(pending.peer).await?;
    Ok(())
}
