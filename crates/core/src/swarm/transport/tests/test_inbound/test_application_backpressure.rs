//! A full application inbox (#913 R10 H1, H2, law L0): core drops further live application
//! data, counted, and no lane waits for the application to read.
//!
//! ```text
//!   peer ──App × (budget + k)──▶ handled ──▶ inbox full: dropped, counted
//!        ──Chunk…Chunk──▶ reassembled ──▶ inbox full: dropped, counted
//!        ──DhtControl / Storage──▶ handled, behind nothing
//!   app reads ──▶ room ──▶ the next App frame is queued again
//! ```

use std::sync::Arc;

use futures::channel::mpsc;

use super::super::transport_with_measure;
use super::super::RecordingMeasure;
use super::deliver_chunked;
use super::local_wire;
use super::next_custom_data;
use super::next_tapped;
use super::next_tapped_custom;
use super::noop_control_message;
use super::spawn_inbound_delivery;
use crate::delegation::DelegateeKey;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::Did;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::CustomMessage;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::Delivered;
use crate::swarm::application::APPLICATION_INBOX_WEIGHT;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::swarm::callback::SwarmTransportCallback;
use crate::swarm::transport::SwarmTransport;
use crate::tests::live_entry;

/// The ring positions of the entries the storage frame of these tests places locally.
const STORED_POSITIONS: [u32; 2] = [41, 42];

/// The transaction id of the filler [`claim_full_inbox`] queues.
const FILLER_TX: uuid::Uuid = uuid::Uuid::nil();

/// The leading bytes of the application frames one peer carries past its inbound budget.
const BURST_PREFIX: &[u8] = b"burst/";

/// The application frames one peer sends beyond its inbound budget.
const BEYOND_BUDGET: usize = 8;

/// A storage frame placing one live entry at each of [`STORED_POSITIONS`] on `local`.
fn storage_message(local: Did) -> Message {
    let entries = STORED_POSITIONS
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

/// Claim `transport`'s application inbox and fill it: one live message of the whole
/// [`APPLICATION_INBOX_WEIGHT`], queued into the empty inbox, leaves room for no other.
fn claim_full_inbox(transport: &SwarmTransport) -> Result<ApplicationInbox> {
    let inbox = transport.application().outlet.claim()?;
    let filler = ApplicationMessage {
        tx_id: FILLER_TX,
        from: transport.dht.did,
        data: ApplicationData::Custom(CustomMessage(vec![0; APPLICATION_INBOX_WEIGHT])),
    };
    match transport.application().outlet.deliver(filler) {
        Delivered::Queued => Ok(inbox),
        other => Err(Error::InvalidMessage(format!(
            "an empty claimed inbox must queue the filler, but it was {other:?}"
        ))),
    }
}

/// Every payload the inbound tap has observed so far, as messages, without waiting.
fn drain_tapped(tap: &mut mpsc::UnboundedReceiver<MessagePayload>) -> Result<Vec<Message>> {
    std::iter::from_fn(|| tap.try_recv().ok())
        .map(|payload| payload.transaction.data())
        .collect()
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

/// Law (H1, no lane waits): with the inbox full, one peer's application frames beyond its
/// inbound budget are each handled and dropped, counted, and the same peer's control frame
/// sent behind them is handled; nothing stays admitted.
#[tokio::test]
async fn test_full_inbox_drops_application_frames_beyond_the_peer_budget() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let local = transport.dht.did;
    let inbox = claim_full_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(SwarmTransportCallback::new(Arc::clone(&transport)));
    let cid = peer.to_string();
    let sent = inbound_peer_capacity_for_test() + BEYOND_BUDGET;

    // Each frame has an origin of its own, so no origin's message burst refuses it before the
    // inbox: the budget they share is the carrying peer's.
    let deliveries = (0..sent)
        .map(|index| {
            let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
            let body = [BURST_PREFIX, index.to_string().as_bytes()].concat();
            let frame = local_wire(Message::custom(&body)?, &origin, local)?;
            Ok(spawn_inbound_delivery(
                Arc::clone(&callback),
                cid.clone(),
                frame,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    // The control frame is sent once the application frames are entering the logical stage.
    assert!(next_tapped_custom(&mut tap)
        .await?
        .starts_with(BURST_PREFIX));
    callback
        .on_admitted_message_for_test(
            &cid,
            &local_wire(noop_control_message(local), &session, local)?,
        )
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    await_deliveries(deliveries).await?;

    let tapped = drain_tapped(&mut tap)?;
    assert_eq!(
        tapped
            .iter()
            .filter(|message| matches!(message, Message::FindSuccessorReport(_)))
            .count(),
        1
    );
    assert_eq!(
        tapped
            .iter()
            .filter(|message| matches!(message, Message::CustomMessage(_)))
            .count(),
        sent - 1
    );
    assert_eq!(
        inbox.dropped(),
        u64::try_from(sent).map_err(|_| Error::MessageSizeOverflow)?
    );
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

/// Law (H2, no hand-off barrier waits): with the inbox full, a chunked application message is
/// reassembled, handed off and dropped, counted, and the same peer's storage frame sent behind
/// it is handled next, its entries reaching the DHT.
#[tokio::test]
async fn test_full_inbox_drops_a_reassembled_message_without_stalling_storage() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let local = transport.dht.did;
    let inbox = claim_full_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(SwarmTransportCallback::new(Arc::clone(&transport)));
    let cid = peer.to_string();

    // The final chunk completes the message, whose hand-off to the Application lane is in
    // flight while the storage frame arrives behind it.
    let (final_delivery, chunk_count) = deliver_chunked(
        &callback,
        &cid,
        &session,
        local,
        Message::custom(b"reassembled-and-dropped")?,
    )
    .await?;
    let storage = local_wire(storage_message(local), &session, local)?;
    spawn_inbound_delivery(Arc::clone(&callback), cid, storage)
        .await
        .map_err(|_| Error::InvalidMessage("storage delivery task panicked".to_string()))??;
    await_deliveries(vec![final_delivery]).await?;

    for _ in 0..chunk_count {
        assert!(matches!(next_tapped(&mut tap).await?, Message::Chunk(_)));
    }
    assert_eq!(
        next_tapped_custom(&mut tap).await?,
        b"reassembled-and-dropped".to_vec()
    );
    assert!(matches!(
        next_tapped(&mut tap).await?,
        Message::SyncEntriesWithSuccessor(_)
    ));
    for position in STORED_POSITIONS {
        assert!(transport
            .dht
            .storage
            .get(&Did::from(position).to_string())
            .await?
            .is_some());
    }
    assert_eq!(inbox.dropped(), 1);
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}

/// Deliver a custom message of `body` from `session` to `local` over the connection `cid`,
/// awaiting its handling.
async fn deliver_custom(
    callback: &SwarmTransportCallback,
    cid: &str,
    session: &DelegateeKey,
    local: Did,
    body: &[u8],
) -> Result<()> {
    callback
        .on_admitted_message_for_test(cid, &local_wire(Message::custom(body)?, session, local)?)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))
}

/// Law (reading makes room): an application frame arriving at a full inbox is dropped; once the
/// application reads the filler, the next application frame is queued and read.
#[tokio::test]
async fn test_reading_the_inbox_makes_room_for_the_next_application_frame() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let cid = Did::from(peer_key.address()).to_string();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let local = transport.dht.did;
    let inbox = claim_full_inbox(&transport)?;
    let callback = SwarmTransportCallback::new(Arc::clone(&transport));

    deliver_custom(&callback, &cid, &session, local, b"dropped-while-full").await?;
    assert_eq!(inbox.dropped(), 1);
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(ApplicationMessage {
            tx_id: FILLER_TX,
            ..
        }))
    ));
    deliver_custom(&callback, &cid, &session, local, b"queued-after-read").await?;
    assert_eq!(inbox.dropped(), 1);
    assert_eq!(
        next_custom_data(&inbox).await?,
        b"queued-after-read".to_vec()
    );
    assert!(inbox.try_next().is_none());
    Ok(())
}
