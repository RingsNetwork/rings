use std::time::Duration;

use tracing_test::traced_test;

use super::*;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::RelayCustody;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::message::ConnectNodeSend;
use crate::message::FetchRelayInbox;
use crate::message::PeerWireVersion;
use crate::message::SyncEntriesWithSuccessor;
use crate::message::WireVersionMismatch;
use crate::message::WIRE_VERSION;
use crate::swarm::callback::inbound_application_capacity_for_test;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::tests::live_entry;

mod test_admission_delegation;
mod test_application_backpressure;
mod test_capacity_handoff;
mod test_delegation_reference;
mod test_link_replay;
mod test_origin_quota;
mod test_pre_admission;
mod test_reassembly;
mod test_storage_interleave;

/// The frame-header law on the overlay path (#1104): an overlay handshake (`ConnectNodeSend`)
/// that arrives on an admitted link in a frame of another wire version is refused before its
/// body is decoded, naming both versions, and reaches no handler.
#[tokio::test]
async fn test_a_link_frame_of_another_wire_version_is_refused_before_decoding() -> Result<()> {
    let transport = default_transport()?;
    let recorder = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let offer = Message::ConnectNodeSend(ConnectNodeSend {
        sdp: String::new(),
        dht_protocol_mode: transport.dht_protocol_mode(),
        relay_custody: RelayCustody::Holds,
    });
    let theirs = WIRE_VERSION + 1;
    let mut frame = local_wire(offer, &pending.session, transport.dht.did)?.to_vec();
    frame[6..8].copy_from_slice(&theirs.to_be_bytes());

    let refused = pending
        .callback
        .on_admitted_message_for_test(&pending.peer.to_string(), &frame)
        .await;
    assert!(matches!(
        refused.as_ref().map_err(|error| error.downcast_ref::<Error>()),
        Err(Some(Error::IncompatibleWireVersion(WireVersionMismatch {
            ours: WIRE_VERSION,
            theirs: PeerWireVersion::Versioned(version),
        }))) if *version == theirs
    ));
    assert_eq!(recorder.observed(), 0);
    Ok(())
}

/// A view refusal is the protocol's answer, not a fault (#1151): a `FetchRelayInbox` reaching a
/// node that keeps no relay store, so its view makes it no member of the recipient's inbox key,
/// is refused with `RelayCarrierNotHeldHere`, logged once at debug and nowhere at error, and the
/// transport is told the frame was handled.
#[traced_test]
#[tokio::test]
async fn test_a_view_refusal_is_logged_once_at_debug_and_answered() -> Result<()> {
    let transport = default_transport()?;
    let pending = pending_peer(&transport).await?;
    pending.admit(&transport).await?;
    let fetch = local_wire(
        Message::FetchRelayInbox(FetchRelayInbox),
        &pending.session,
        transport.dht.did,
    )?;

    pending.receive(&fetch).await?;

    logs_assert(|lines: &[&str]| {
        let refusals = lines
            .iter()
            .filter(|line| line.contains("RelayCarrierNotHeldHere"))
            .collect::<Vec<_>>();
        match refusals.as_slice() {
            [refusal] if refusal.contains("DEBUG") => Ok(()),
            _ => Err(format!("expected one debug refusal, logged {refusals:?}")),
        }
    });
    Ok(())
}

/// Deliver `early` from `pending`'s peer before this end admits the handshake, where it is held
/// unobserved, then admit the handshake on a data channel that opens while ICE still connects.
/// Fails if the open waits on the delivery of the held frame.
async fn admit_after_one_held_frame(
    transport: &SwarmTransport,
    pending: &PendingPeer,
    recorder: &ApplicationRecorder,
    early: &[u8],
) -> Result<()> {
    pending
        .receive(&pending.custom_message_wire(transport, early)?)
        .await?;
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 1);
    assert_eq!(recorder.observed(), 0);

    transport.force_peer_connection_state_without_callback(
        pending.peer,
        WebrtcConnectionState::Connecting,
    )?;
    transport.force_peer_data_channel_open_without_callback(pending.peer, Some(true))?;
    let callback = Arc::clone(&pending.callback);
    let cid = pending.peer.to_string();
    let open = tokio::spawn(async move {
        callback
            .on_data_channel_open(&cid)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });
    tokio::time::timeout(Duration::from_secs(1), open)
        .await
        .map_err(|_| Error::InvalidMessage("data-channel open waited on the held frame".into()))?
        .map_err(|_| Error::InvalidMessage("data-channel-open task panicked".to_string()))?
}

/// The drain of pre-admission frames must not block the data-channel open, and an
/// arrival admitted after it is queued in order behind the drained frame: both reach
/// the application once the lane is released.
#[tokio::test]
async fn test_pre_admission_drain_does_not_block_open_and_orders_admitted_arrivals() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let pending = pending_peer(&transport).await?;
    let application_admission = pending
        .callback
        .inbound_for_test()
        .hold_application_admission_for_test()?;
    admit_after_one_held_frame(
        &transport,
        &pending,
        &application,
        b"early-drain-must-not-block-open",
    )
    .await?;

    let late =
        pending.custom_message_wire(&transport, b"late-admitted-arrival-queues-behind-drain")?;
    let late_delivery = spawn_inbound_delivery(
        Arc::clone(&pending.callback),
        pending.peer.to_string(),
        late,
    );
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
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 0);
    assert_eq!(application.inbound_custom_data(), vec![
        b"early-drain-must-not-block-open".to_vec(),
        b"late-admitted-arrival-queues-behind-drain".to_vec(),
    ]);

    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// The drain of pre-admission frames returns once the frames are submitted, not once their
/// application delivery completes: with the Application lane held, the data-channel open
/// completes while the drained frame waits at the logical stage, and releasing the lane
/// delivers it.
#[tokio::test]
async fn test_pre_admission_drain_returns_before_application_delivery_completes() -> Result<()> {
    let transport = default_transport()?;
    let (application, lane_hold) =
        ApplicationRecorder::attach_holding_application_lane(&transport)?;
    let pending = pending_peer(&transport).await?;
    let body = b"early-delivery-must-not-block-admission";
    admit_after_one_held_frame(&transport, &pending, &application, body).await?;

    assert!(transport.has_active_connection(pending.peer));
    assert!(transport.is_active_connection_attempt(pending.attempt()?));
    let drained = tokio::time::timeout(Duration::from_secs(1), application.next_tapped_custom())
        .await
        .map_err(|_| {
            Error::InvalidMessage("held frame was not submitted to inbound actor".to_string())
        })??;
    assert_eq!(drained, body.to_vec());
    assert_eq!(pending.callback.pre_admission_held_count_for_test(), 0);
    drop(lane_hold);
    application.wait_for_inbounds_at_least(1).await;
    assert_eq!(application.inbound_custom_data(), vec![body.to_vec()]);

    transport.disconnect(pending.peer).await?;
    Ok(())
}

/// With the application lane saturated (every permit admitted and the lane
/// held, so no frame is dispatched), work beyond the mailbox capacity is
/// rejected with the typed error while a control frame is still admitted and
/// delivered on its own lane; releasing the lane drains every held frame.
#[tokio::test]
async fn test_inbound_mailbox_reserves_control_capacity_under_application_saturation() -> Result<()>
{
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    let callback = Arc::new(SwarmTransportCallback::new(Arc::clone(&transport)));
    // Held for the whole saturation: the lane's front sequence never becomes
    // ready, so the actor dispatches no application frame and no permit is
    // released while the capacity laws are observed.
    let application_hold = callback
        .inbound_for_test()
        .hold_application_admission_for_test()?;
    let application_inputs = saturating_application_inputs(transport.dht.did)?;
    let control_peer = unbound_peer(&transport, &SecretKey::random())?;
    let overflow_message = local_wire(
        Message::custom(b"global-application-overflow")?,
        &control_peer.session,
        transport.dht.did,
    )?;
    let control = local_wire(
        noop_control_message(transport.dht.did),
        &control_peer.session,
        transport.dht.did,
    )?;
    let control_cid = control_peer.peer.to_string();
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
        .inbound_for_test()
        .await_admitted_count_for_test(|admitted| {
            admitted >= inbound_application_capacity_for_test()
        })
        .await;

    // Work beyond the active and queued capacity is not refused: it waits, holding its
    // sender's credit, until the mailbox drains.
    let overflow =
        spawn_inbound_delivery(Arc::clone(&callback), control_cid.clone(), overflow_message);
    callback
        .inbound_for_test()
        .await_waiting_for_test(|waiting| waiting == 1)
        .await;

    // The reserved control lane is independent of the held application lane, and of the
    // same peer's waiting application arrival: the control frame is admitted and handled while
    // every application permit stays held and the overflow still waits.
    callback
        .on_admitted_message_for_test(&control_cid, &control)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    assert_eq!(application.observed(), 1);
    assert_eq!(
        callback.inbound_for_test().admitted_count_for_test(),
        inbound_application_capacity_for_test()
    );
    assert!(!overflow.is_finished());

    drop(application_hold);
    deliveries.push(overflow);
    await_deliveries(deliveries).await?;
    assert_eq!(callback.inbound_for_test().admitted_count_for_test(), 0);
    Ok(())
}

/// Closing the inbound mailbox cancels the work in flight: an application delivery waiting at
/// its held lane fails with the closed mailbox, releases its permit, and queues nothing.
#[tokio::test]
async fn test_closing_inbound_mailbox_cancels_waiting_application_delivery() -> Result<()> {
    let transport = default_transport()?;
    let (application, lane_hold) =
        ApplicationRecorder::attach_holding_application_lane(&transport)?;
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let message = peer.custom_message_wire(&transport, b"waiting-application-delivery")?;
    let cid = peer.peer.to_string();
    let task_callback = Arc::clone(&peer.callback);
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

    // Observed, the frame waits at its held lane, just before dispatch.
    application.next_tapped().await?;
    peer.callback.inbound_for_test().close_for_test();
    assert!(delivery
        .await
        .map_err(|_| Error::InvalidMessage("inbound mailbox task panicked".to_string()))?);
    assert_eq!(
        peer.callback.inbound_for_test().admitted_count_for_test(),
        0
    );
    drop(lane_hold);
    assert_eq!(application.inbounds(), 0);
    Ok(())
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

/// A storage frame placing one live entry at each of `positions` on `local`.
fn storage_message(local: Did, positions: [u32; 2]) -> Message {
    let entries = positions
        .into_iter()
        .map(|position| {
            let entry = live_entry(Did::from(position), Vec::new(), EntryKind::Data);
            PlacedEntry::new(entry.did, entry)
        })
        .collect();
    Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(local),
        data: entries,
    })
}

/// A DHT control message reporting `did` as a successor to no handler: handled, it changes
/// nothing.
fn noop_control_message(did: Did) -> Message {
    Message::FindSuccessorReport(crate::message::FindSuccessorReport {
        did,
        handler: crate::message::FindSuccessorReportHandler::None,
    })
}

/// Deliver `wire` to `local` as chunks of 32 bytes on `carrier`, each signed by `session`:
/// every chunk but the last in turn. Returns the last chunk's frame, undelivered, and the number
/// of chunks.
async fn deliver_leading_chunks(
    callback: &SwarmTransportCallback,
    (cid, carrier): (&str, Option<ChannelLane>),
    session: &DelegateeKey,
    local: Did,
    wire: Bytes,
) -> Result<(Bytes, usize)> {
    let mut chunks: Vec<Chunk> = Chunk::stream(wire, 32).collect();
    let count = chunks.len();
    let last = chunks.pop().ok_or(Error::InboundActorInvariantViolation)?;
    assert!(!chunks.is_empty(), "the message spans several chunks");
    for chunk in chunks {
        callback
            .on_carried_message_for_test(
                cid,
                &local_wire(Message::Chunk(chunk), session, local)?,
                carrier,
            )
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    }
    Ok((local_wire(Message::Chunk(last), session, local)?, count))
}

/// Deliver `message`, signed by `session` and addressed to `local`, as chunks of 32 bytes on
/// `carrier`: every chunk but the last in turn, then the last on its own task, whose delivery
/// completes the message. Returns that delivery and the number of chunks.
async fn deliver_chunked(
    callback: &Arc<SwarmTransportCallback>,
    (cid, carrier): (&str, Option<ChannelLane>),
    session: &DelegateeKey,
    local: Did,
    message: Message,
) -> Result<(tokio::task::JoinHandle<Result<()>>, usize)> {
    let wire = local_wire(message, session, local)?;
    let (last, count) =
        deliver_leading_chunks(callback, (cid, carrier), session, local, wire).await?;
    Ok((
        spawn_carried_delivery(Arc::clone(callback), cid.to_string(), last, carrier),
        count,
    ))
}

/// Await every spawned inbound delivery, each of which must succeed.
async fn await_deliveries(deliveries: Vec<tokio::task::JoinHandle<Result<()>>>) -> Result<()> {
    for delivery in deliveries {
        delivery
            .await
            .map_err(|_| Error::InvalidMessage("inbound delivery task panicked".to_string()))??;
    }
    Ok(())
}

fn spawn_inbound_delivery(
    callback: Arc<SwarmTransportCallback>,
    cid: String,
    frame: bytes::Bytes,
) -> tokio::task::JoinHandle<Result<()>> {
    spawn_carried_delivery(callback, cid, frame, None)
}

/// Deliver `frame` from `cid` on its own task, as if it arrived on the transport lane
/// `carrier`.
fn spawn_carried_delivery(
    callback: Arc<SwarmTransportCallback>,
    cid: String,
    frame: bytes::Bytes,
    carrier: Option<ChannelLane>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        callback
            .on_carried_message_for_test(&cid, &frame, carrier)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    })
}

fn hold_saturated_application_capacity(
    callback: &SwarmTransportCallback,
) -> Result<Vec<impl Sized>> {
    let lane_capacity = inbound_application_capacity_for_test();
    let peer_capacity = inbound_peer_capacity_for_test();
    let mut capacity_holds = Vec::with_capacity(lane_capacity);
    while capacity_holds.len() < lane_capacity {
        let peer: Did = SecretKey::random().address().into();
        for _ in 0..peer_capacity.min(lane_capacity - capacity_holds.len()) {
            capacity_holds.push(
                callback
                    .inbound_for_test()
                    .hold_application_capacity_for_test(peer)?,
            );
        }
    }
    assert_eq!(
        callback.inbound_for_test().admitted_count_for_test(),
        lane_capacity
    );
    Ok(capacity_holds)
}

mod measurement;
