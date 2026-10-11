use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;

use super::*;
use crate::message::CustomMessage;
use crate::message::FoundEntry;
use crate::message::MessageSigner;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::APPLICATION_INBOX_WEIGHT;
use crate::swarm::callback::inbound_application_capacity_for_test;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::swarm::callback::InboundLane;
use crate::tests::TEST_NETWORK_ID;

mod test_admission_delegation;
mod test_application_backpressure;
mod test_capacity_handoff;
mod test_delegation_reference;
mod test_origin_quota;
mod test_pre_admission;
mod test_reassembly;
mod test_storage_interleave;

/// The transaction id of the filler [`claim_full_application_inbox`] queues.
const INBOX_FILLER_TX: uuid::Uuid = uuid::Uuid::nil();

/// Claim `transport`'s application inbox and fill it to [`APPLICATION_INBOX_WEIGHT`] with one
/// filler message: until the filler is read, every application-data delivery waits for room,
/// and only the `Application` and `E2e` lanes with it.
fn claim_full_application_inbox(transport: &SwarmTransport) -> Result<ApplicationInbox> {
    let inbox = transport.application().outlet.claim()?;
    let filler = ApplicationMessage {
        tx_id: INBOX_FILLER_TX,
        from: transport.dht.did,
        data: ApplicationData::Custom(CustomMessage(vec![0; APPLICATION_INBOX_WEIGHT])),
    };
    transport
        .application()
        .outlet
        .try_reserve(&filler)
        .ok_or_else(|| Error::InvalidMessage("an empty inbox has room".to_string()))?
        .fill(filler);
    Ok(inbox)
}

/// Read the filler of [`claim_full_application_inbox`], making room for the waiting deliveries.
/// Peer facts queued ahead of it (the inbox yields events first) are read past.
fn release_full_application_inbox(inbox: &ApplicationInbox) -> Result<()> {
    loop {
        match inbox.try_next() {
            Some(ApplicationDelivery::Event(_)) => continue,
            Some(ApplicationDelivery::Message(message)) if message.tx_id == INBOX_FILLER_TX => {
                return Ok(());
            }
            _ => {
                return Err(Error::InvalidMessage(
                    "the inbox filler is not the first message".to_string(),
                ))
            }
        }
    }
}

/// The body of the next custom message `inbox` yields, waiting for it.
async fn next_custom_data(inbox: &ApplicationInbox) -> Result<Vec<u8>> {
    match inbox.next().await {
        Some(ApplicationDelivery::Message(ApplicationMessage {
            data: ApplicationData::Custom(CustomMessage(body)),
            ..
        })) => Ok(body),
        other => Err(Error::InvalidMessage(format!(
            "expected a custom message, found {other:?}"
        ))),
    }
}

/// The message of the next payload the inbound tap observed entering the logical stage,
/// waiting for it.
async fn next_tapped(tap: &mut mpsc::UnboundedReceiver<MessagePayload>) -> Result<Message> {
    tap.next()
        .await
        .ok_or_else(|| Error::InvalidMessage("inbound tap closed".to_string()))?
        .transaction
        .data()
}

/// The body of the next payload the inbound tap observed, which must be a custom message,
/// waiting for it.
async fn next_tapped_custom(tap: &mut mpsc::UnboundedReceiver<MessagePayload>) -> Result<Vec<u8>> {
    match next_tapped(tap).await? {
        Message::CustomMessage(CustomMessage(body)) => Ok(body),
        other => Err(Error::InvalidMessage(format!(
            "expected a custom message, found {other:?}"
        ))),
    }
}

/// The drain of pre-admission frames must not block the data-channel open, and an
/// arrival admitted after it is queued in order behind the drained frame: both reach
/// the application once the lane is released.
#[tokio::test]
async fn test_pre_admission_drain_does_not_block_open_and_orders_admitted_arrivals() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let application = ApplicationRecorder::attach(&transport)?;
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport)).with_pending_connection_attempt(attempt),
    );
    let early = MessagePayload::new_send(
        Message::custom(b"early-drain-must-not-block-open")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let late = MessagePayload::new_send(
        Message::custom(b"late-admitted-arrival-queues-behind-drain")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    callback
        .on_admitted_message_for_test(&cid, &early)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 1);
    let application_admission = callback.hold_application_admission_for_test()?;

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connecting)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    let open_callback = Arc::clone(&callback);
    let open_cid = cid.clone();
    let open = tokio::spawn(async move {
        open_callback
            .on_data_channel_open(&open_cid)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), open)
        .await
        .map_err(|_| Error::InvalidMessage("data-channel open waited on drain".to_string()))?
        .map_err(|_| Error::InvalidMessage("data-channel-open task panicked".to_string()))??;

    let late_callback = Arc::clone(&callback);
    let late_cid = cid.clone();
    let late_delivery = tokio::spawn(async move {
        late_callback
            .on_admitted_message_for_test(&late_cid, &late)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    // The late arrival's completion is the actor's dispatch, which the held
    // lane defers: nothing reaches the application until the lane is released.
    assert_eq!(application.inbounds(), 0);

    drop(application_admission);
    tokio::time::timeout(Duration::from_secs(1), late_delivery)
        .await
        .map_err(|_| {
            Error::InvalidMessage("admitted arrival waited beyond the lane release".to_string())
        })?
        .map_err(|_| Error::InvalidMessage("admitted-arrival task panicked".to_string()))??;
    application.wait_for_inbounds_at_least(2).await;
    assert_eq!(callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(application.inbound_custom_data(), vec![
        b"early-drain-must-not-block-open".to_vec(),
        b"late-admitted-arrival-queues-behind-drain".to_vec(),
    ]);

    transport.disconnect(peer).await?;
    Ok(())
}

/// The drain of pre-admission frames returns once the frames are submitted, not once the
/// application has room for them: with the inbox full, the data-channel open completes while
/// the drained frame waits for room, and reading the inbox delivers it.
#[tokio::test]
async fn test_pre_admission_drain_returns_before_application_delivery_completes() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let inbox = claim_full_application_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let callback = Arc::new(
        InnerSwarmCallback::new(Arc::clone(&transport)).with_pending_connection_attempt(attempt),
    );
    let body = b"early-delivery-must-not-block-admission";
    let message = MessagePayload::new_send(
        Message::custom(body)?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();

    callback
        .on_admitted_message_for_test(&cid, &message)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(callback.pre_admission_held_count_for_test(), 1);
    assert!(tap.try_recv().is_err());

    transport
        .force_peer_connection_state_without_callback(peer, WebrtcConnectionState::Connecting)?;
    transport.force_peer_data_channel_open_without_callback(peer, Some(true))?;
    let open_callback = Arc::clone(&callback);
    let open_cid = cid.clone();
    let open = tokio::spawn(async move {
        open_callback
            .on_data_channel_open(&open_cid)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), open)
        .await
        .map_err(|_| Error::InvalidMessage("data-channel open waited on delivery".to_string()))?
        .map_err(|_| Error::InvalidMessage("data-channel-open task panicked".to_string()))??;

    assert!(transport.has_active_connection(peer));
    assert!(transport.is_active_connection_attempt(attempt));
    let drained = tokio::time::timeout(Duration::from_secs(1), next_tapped_custom(&mut tap))
        .await
        .map_err(|_| {
            Error::InvalidMessage("held frame was not submitted to inbound actor".to_string())
        })??;
    assert_eq!(drained, body.to_vec());
    assert_eq!(callback.pre_admission_held_count_for_test(), 0);
    release_full_application_inbox(&inbox)?;
    assert_eq!(next_custom_data(&inbox).await?, body.to_vec());

    transport.disconnect(peer).await?;
    Ok(())
}

/// With the application lane saturated (every permit admitted and the lane
/// held, so no frame is dispatched), work beyond the mailbox capacity is
/// rejected with the typed error while a control frame is still admitted and
/// delivered on its own lane; releasing the lane drains every held frame.
#[tokio::test]
async fn test_inbound_mailbox_reserves_control_capacity_under_application_saturation() -> Result<()>
{
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let application = ApplicationRecorder::attach(&transport)?;
    let callback = Arc::new(InnerSwarmCallback::new(Arc::clone(&transport)));
    // Held for the whole saturation: the lane's front sequence never becomes
    // ready, so the actor dispatches no application frame and no permit is
    // released while the capacity laws are observed.
    let application_hold = callback.hold_application_admission_for_test()?;
    let application_inputs = saturating_application_inputs(transport.dht.did)?;
    let control_key = SecretKey::random();
    let control_peer: Did = control_key.address().into();
    let control_session = DelegateeKey::new_with_seckey(&control_key)?;
    let overflow_message = local_wire(
        Message::custom(b"global-application-overflow")?,
        &control_session,
        transport.dht.did,
    )?;
    let control = local_wire(
        noop_control_message(transport.dht.did),
        &control_session,
        transport.dht.did,
    )?;
    let control_cid = control_peer.to_string();
    let mut deliveries = Vec::new();

    for (cid, messages) in application_inputs {
        for message in messages {
            deliveries.push(spawn_inbound_delivery(
                Arc::clone(&callback),
                cid.clone(),
                message,
            ));
        }
    }
    callback
        .await_inbound_admitted_count_for_test(|admitted| {
            admitted >= inbound_application_capacity_for_test()
        })
        .await;

    // Work beyond the active and queued capacity is not refused: it waits, holding its
    // sender's credit, until the mailbox drains.
    let overflow =
        spawn_inbound_delivery(Arc::clone(&callback), control_cid.clone(), overflow_message);
    callback
        .await_inbound_waiting_for_test(|waiting| waiting == 1)
        .await;

    // The reserved control lane is independent of the held application lane, and of the
    // same peer's waiting application arrival: the control frame is admitted and handled while
    // every application permit stays held and the overflow still waits.
    callback
        .on_admitted_message_for_test(&control_cid, &control)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(application.validates(), 1);
    assert_eq!(
        callback.inbound_admitted_count_for_test(),
        inbound_application_capacity_for_test()
    );
    assert!(!overflow.is_finished());

    drop(application_hold);
    for delivery in deliveries {
        delivery
            .await
            .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))??;
    }
    overflow
        .await
        .map_err(|_| Error::InvalidMessage("inbound overflow task panicked".to_string()))??;
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

/// Closing the inbound mailbox cancels the work in flight: an application delivery waiting for
/// inbox room fails with the closed mailbox, releases its permit, and queues nothing.
#[tokio::test]
async fn test_closing_inbound_mailbox_cancels_waiting_application_delivery() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let inbox = claim_full_application_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(InnerSwarmCallback::new(Arc::clone(&transport)));
    let message = MessagePayload::new_send(
        Message::custom(b"waiting-application-delivery")?,
        MessageSigner::new(&peer_session, TEST_NETWORK_ID),
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    let cid = peer.to_string();
    let task_callback = Arc::clone(&callback);
    let delivery = tokio::spawn(async move {
        match task_callback
            .on_admitted_message_for_test(&cid, &message)
            .await
        {
            Err(error) => matches!(
                error.downcast_ref::<Error>(),
                Some(Error::InboundMailboxClosed)
            ),
            Ok(()) => false,
        }
    });

    // Handled, the frame waits for inbox room: nothing between the tap and the inbox suspends.
    next_tapped(&mut tap).await?;
    callback.close_inbound_for_test();
    assert!(delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))?);
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    release_full_application_inbox(&inbox)?;
    assert!(inbox.try_next().is_none());
    Ok(())
}

fn local_wire(message: Message, session: &DelegateeKey, local: Did) -> Result<bytes::Bytes> {
    MessagePayload::new_send(
        message,
        MessageSigner::new(session, TEST_NETWORK_ID),
        local,
        local,
    )?
    .to_wire()
}

/// One full peer budget of application frames from each of the peers that together fill the
/// application capacity of `local`'s inbound mailbox, as `(cid, frames)`.
fn saturating_application_inputs(local: Did) -> Result<Vec<(String, Vec<bytes::Bytes>)>> {
    let peer_count = inbound_application_capacity_for_test() / inbound_peer_capacity_for_test();
    assert_eq!(
        peer_count * inbound_peer_capacity_for_test(),
        inbound_application_capacity_for_test()
    );
    (0..peer_count)
        .map(|_| {
            let key = SecretKey::random();
            let peer: Did = key.address().into();
            let session = DelegateeKey::new_with_seckey(&key)?;
            let frames = (0..inbound_peer_capacity_for_test())
                .map(|_| {
                    local_wire(
                        Message::custom(b"bounded-inbound-mailbox")?,
                        &session,
                        local,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((peer.to_string(), frames))
        })
        .collect()
}

fn noop_control_message(did: Did) -> Message {
    Message::FindSuccessorReport(crate::message::FindSuccessorReport {
        did,
        handler: crate::message::FindSuccessorReportHandler::None,
    })
}

fn failed_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_counters()?
        .into_iter()
        .filter(|(did, counter)| *did == peer && *counter == MeasurementEvent::FailedToReceive)
        .count())
}

fn successful_receive_count(measure: &RecordingMeasure, peer: Did) -> Result<usize> {
    Ok(measure
        .snapshot_measurements()?
        .into_iter()
        .filter(|(did, event)| *did == peer && matches!(event, MeasurementEvent::Received { .. }))
        .count())
}

fn spawn_inbound_delivery(
    callback: Arc<InnerSwarmCallback>,
    cid: String,
    frame: bytes::Bytes,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        callback
            .on_admitted_message_for_test(&cid, &frame)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    })
}

fn hold_saturated_application_capacity(callback: &InnerSwarmCallback) -> Result<Vec<impl Drop>> {
    let lane_capacity = inbound_application_capacity_for_test();
    let peer_capacity = inbound_peer_capacity_for_test();
    let mut capacity_holds = Vec::with_capacity(lane_capacity);
    while capacity_holds.len() < lane_capacity {
        let peer: Did = SecretKey::random().address().into();
        for _ in 0..peer_capacity.min(lane_capacity - capacity_holds.len()) {
            capacity_holds.push(callback.hold_application_capacity_for_test(peer)?);
        }
    }
    assert_eq!(callback.inbound_admitted_count_for_test(), lane_capacity);
    Ok(capacity_holds)
}

mod measurement;
