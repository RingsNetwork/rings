//! The link stage of one inbound connection: frames whose session slots are references, the
//! hold behind a miss, and the link-control frames that repair it. Every wait is on the
//! application callback's delivery event.

use std::borrow::Cow;

use super::*;
use crate::message::MessagePayload;
use crate::message::PerSlot;
use crate::message::SessionControl;
use crate::message::SessionRef;
use crate::message::WirePayload;

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
        origin: SessionRef::Inline(Cow::Borrowed(sessions.origin)),
        hop: SessionRef::Digest(sessions.hop.digest()?),
    };
    WirePayload::view(payload, references)
        .to_wire()
        .map(|wire| wire.to_vec())
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

/// Miss, then announcement: a frame that references sessions this connection never carried is
/// held, not failed; frames behind it wait their turn; the peer's announcement releases them in
/// arrival order.
#[tokio::test]
async fn test_missed_session_holds_frames_until_the_peer_announces_it() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let missed = custom_payload(&pending, &transport, b"missed")?;
    pending.receive(&referenced_wire(&missed)?).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"behind")?)
        .await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 2);
    assert_eq!(app_callback.inbounds(), 0);
    assert!(!measure
        .snapshot_counters()?
        .contains(&(pending.peer, MeasureCounter::FailedToReceive)));

    let announcement = SessionControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(2).await;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"missed".to_vec(),
        b"behind".to_vec(),
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// Miss on the hop slot alone is held and repaired the same way.
#[tokio::test]
async fn test_missed_hop_session_is_repaired_by_announcement() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let missed = custom_payload(&pending, &transport, b"hop-missed")?;
    pending.receive(&hop_referenced_wire(&missed)?).await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 1);

    let announcement = SessionControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(1).await;

    assert_eq!(app_callback.inbound_custom_data()?, vec![
        b"hop-missed".to_vec()
    ]);
    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// A peer that disclaims the session it referenced fails the frames that await it, and the
/// failure is charged to that peer; the frames behind them are delivered.
#[tokio::test]
async fn test_disclaimed_session_fails_awaiting_frames_and_releases_the_rest() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let pending = pending_peer(&transport, &app_callback).await?;
    pending.admit(&transport).await?;

    let missed = custom_payload(&pending, &transport, b"never-resolved")?;
    let digest = missed.sessions().origin.digest()?;
    pending.receive(&referenced_wire(&missed)?).await?;
    pending
        .receive(&pending.custom_message_wire(&transport, b"behind")?)
        .await?;
    assert_eq!(pending.callback.session_hold_count_for_test(), 2);

    let disclaimer = SessionControl::Unknown(digest).to_wire()?;
    pending.receive(disclaimer.as_ref()).await?;
    app_callback.wait_for_inbounds_at_least(1).await;

    assert_eq!(pending.callback.session_hold_count_for_test(), 0);
    assert_eq!(
        app_callback.inbound_custom_data()?,
        vec![b"behind".to_vec()]
    );
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

    let announcement = SessionControl::Announce(pending.session.session()).to_wire()?;
    pending.receive(announcement.as_ref()).await?;
    let referenced = custom_payload(&pending, &transport, b"referenced")?;
    pending.receive(&referenced_wire(&referenced)?).await?;

    assert_eq!(pending.callback.session_hold_count_for_test(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    transport.disconnect(pending.peer).await?;
    Ok(())
}
