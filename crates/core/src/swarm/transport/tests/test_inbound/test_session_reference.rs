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
use crate::swarm::callback::SESSION_HOLD_CAPACITY;
use crate::swarm::transport::emitted_link_control_for_test;

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
    let emitted_before = emitted_link_control_for_test().len();

    let missed = custom_payload(&pending, &transport, b"hop-missed")?;
    pending.receive(&hop_referenced_wire(&missed)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(
        emitted_link_control_for_test().split_off(emitted_before),
        vec![(pending.peer, LinkControl::Request(digest))]
    );

    let announcement = LinkControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(1).await;

    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"hop-missed".to_vec()
    ]);
    assert_eq!(
        emitted_link_control_for_test().split_off(emitted_before),
        vec![
            (pending.peer, LinkControl::Request(digest)),
            (pending.peer, LinkControl::Known(digest)),
        ]
    );
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Law (bound): a frame that finds the hold full is dropped and charged to the peer, and the
/// oldest held frame's question is asked again; what is held stays held.
#[tokio::test]
async fn test_hold_overflow_drops_the_newcomer_charged_and_asks_the_oldest_question_again(
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
    let emitted_before = emitted_link_control_for_test().len();

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
    assert!(measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));
    assert_eq!(
        emitted_link_control_for_test().last(),
        Some(&(pending.peer, LinkControl::Request(oldest_digest)))
    );
    assert!(!emitted_link_control_for_test()
        .split_off(emitted_before)
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
    let emitted_before = emitted_link_control_for_test().len();
    let referenced = custom_payload(&next, &transport, b"referenced-on-the-next-generation")?;
    next.receive(&referenced_wire(&referenced)?).await?;

    assert_eq!(next.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    assert_eq!(
        emitted_link_control_for_test().split_off(emitted_before),
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
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let now_ms = Arc::new(Mutex::new(crate::utils::get_epoch_ms()));
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    let callback = InnerSwarmCallback::new_with_reassembly_clock_for_test(
        Arc::clone(&transport),
        app_callback.clone(),
        Arc::clone(&now_ms),
    )
    .with_pending_connection_attempt(attempt);

    let stranger = SessionSk::new_with_seckey(&SecretKey::random())?;
    let pending = PendingPeer {
        peer,
        session: peer_session,
        callback,
    };
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
