//! A full application inbox (lossy delivery: core never waits on the application): core drops
//! further live application data, counted, and no lane waits for the application to read.
//!
//! ```text
//!   peer ──App × (budget + k)──▶ handled ──▶ inbox full: dropped, counted
//!        ──Chunk…Chunk──▶ reassembled ──▶ inbox full: dropped, counted
//!        ──DhtControl / Storage──▶ handled, behind nothing
//!   app reads ──▶ room ──▶ the next App frame is queued again
//! ```

use std::sync::Arc;

use super::super::default_transport;
use super::super::local_wire;
use super::super::unbound_peer;
use super::super::ApplicationRecorder;
use super::await_deliveries;
use super::deliver_chunked;
use super::noop_control_message;
use super::spawn_inbound_delivery;
use super::storage_message;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::CustomMessage;
use crate::message::Message;
use crate::swarm::application::ApplicationData;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::application::ApplicationMessage;
use crate::swarm::application::APPLICATION_INBOX_WEIGHT;
use crate::swarm::callback::inbound_peer_capacity_for_test;
use crate::swarm::transport::SwarmTransport;

/// The ring positions of the entries the storage frame of these tests places locally.
const STORED_POSITIONS: [u32; 2] = [41, 42];

/// The transaction id of the filler [`fill_inbox`] queues.
const FILLER_TX: uuid::Uuid = uuid::Uuid::nil();

/// The leading bytes of the application frames one peer carries past its inbound budget.
const BURST_PREFIX: &[u8] = b"burst/";

/// The application frames one peer sends beyond its inbound budget.
const BEYOND_BUDGET: usize = 8;

/// Fill `transport`'s claimed application inbox: one live message of the whole
/// [`APPLICATION_INBOX_WEIGHT`], queued into the empty inbox, leaves room for no other.
fn fill_inbox(transport: &SwarmTransport) {
    transport.application().outlet.deliver(ApplicationMessage {
        tx_id: FILLER_TX,
        from: transport.dht.did,
        data: ApplicationData::Custom(CustomMessage(vec![0; APPLICATION_INBOX_WEIGHT])),
    });
}

/// The body of the next custom message `inbox` yields, waiting for it. Peer facts, which share
/// the inbox's one order with messages, are read past.
async fn next_custom_data(inbox: &ApplicationInbox) -> Result<Vec<u8>> {
    loop {
        match inbox.next().await {
            Some(ApplicationDelivery::Event(_)) => continue,
            Some(ApplicationDelivery::Message(ApplicationMessage {
                data: ApplicationData::Custom(CustomMessage(body)),
                ..
            })) => return Ok(body),
            other => {
                return Err(Error::InvalidMessage(format!(
                    "expected a custom message, found {other:?}"
                )))
            }
        }
    }
}

/// Law (no lane waits): with the inbox full, one peer's application frames beyond its
/// inbound budget are each handled and dropped, counted, and the same peer's control frame
/// sent behind them is handled; nothing stays admitted.
#[tokio::test]
async fn test_full_inbox_drops_application_frames_beyond_the_peer_budget() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    fill_inbox(&transport);
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let (local, callback, cid) = (transport.dht.did, &peer.callback, peer.peer.to_string());
    let sent = inbound_peer_capacity_for_test() + BEYOND_BUDGET;

    // Each frame has an origin of its own, so no origin's message burst refuses it before the
    // inbox: the budget they share is the carrying peer's.
    let deliveries = (0..sent)
        .map(|index| {
            let origin = DelegateeKey::new_with_seckey(&SecretKey::random())?;
            let body = [BURST_PREFIX, index.to_string().as_bytes()].concat();
            let frame = local_wire(Message::custom(&body)?, &origin, local)?;
            Ok(spawn_inbound_delivery(
                Arc::clone(callback),
                cid.clone(),
                frame,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    // The control frame is sent once the application frames are entering the logical stage.
    assert!(application
        .next_tapped_custom()
        .await?
        .starts_with(BURST_PREFIX));
    peer.receive(&local_wire(
        noop_control_message(local),
        &peer.session,
        local,
    )?)
    .await?;
    await_deliveries(deliveries).await?;

    let tapped = application.drain_tapped()?;
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
        application.dropped(),
        u64::try_from(sent).map_err(|_| Error::MessageSizeOverflow)?
    );
    assert_eq!(callback.inbound_for_test().admitted_count_for_test(), 0);
    Ok(())
}

/// Law (no hand-off barrier waits): with the inbox full, a chunked application message is
/// reassembled, handed off and dropped, counted, and the same peer's storage frame sent behind
/// it is handled next, its entries reaching the DHT.
#[tokio::test]
async fn test_full_inbox_drops_a_reassembled_message_without_stalling_storage() -> Result<()> {
    let transport = default_transport()?;
    let application = ApplicationRecorder::attach(&transport)?;
    fill_inbox(&transport);
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let (local, callback, cid) = (transport.dht.did, &peer.callback, peer.peer.to_string());

    // The final chunk completes the message, whose hand-off to the Application lane is in
    // flight while the storage frame arrives behind it.
    let (final_delivery, chunk_count) = deliver_chunked(
        callback,
        &cid,
        &peer.session,
        local,
        Message::custom(b"reassembled-and-dropped")?,
    )
    .await?;
    let storage = local_wire(
        storage_message(local, STORED_POSITIONS),
        &peer.session,
        local,
    )?;
    peer.receive(&storage).await?;
    await_deliveries(vec![final_delivery]).await?;

    for _ in 0..chunk_count {
        assert!(matches!(
            application.next_tapped().await?,
            Message::Chunk(_)
        ));
    }
    assert_eq!(
        application.next_tapped_custom().await?,
        b"reassembled-and-dropped".to_vec()
    );
    assert!(matches!(
        application.next_tapped().await?,
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
    assert_eq!(application.dropped(), 1);
    assert_eq!(callback.inbound_for_test().admitted_count_for_test(), 0);
    Ok(())
}

/// Law (reading makes room): an application frame arriving at a full inbox is dropped; once the
/// application reads the filler, the next application frame is queued and read.
#[tokio::test]
async fn test_reading_the_inbox_makes_room_for_the_next_application_frame() -> Result<()> {
    let transport = default_transport()?;
    let inbox = transport.application().outlet.claim()?;
    fill_inbox(&transport);
    let peer = unbound_peer(&transport, &SecretKey::random())?;

    peer.receive(&peer.custom_message_wire(&transport, b"dropped-while-full")?)
        .await?;
    assert_eq!(inbox.dropped(), 1);
    assert!(matches!(
        inbox.try_next(),
        Some(ApplicationDelivery::Message(ApplicationMessage {
            tx_id: FILLER_TX,
            ..
        }))
    ));
    peer.receive(&peer.custom_message_wire(&transport, b"queued-after-read")?)
        .await?;
    assert_eq!(inbox.dropped(), 1);
    assert_eq!(
        next_custom_data(&inbox).await?,
        b"queued-after-read".to_vec()
    );
    assert!(inbox.try_next().is_none());
    Ok(())
}
