//! Application backpressure (#935, law L0): an application that does not read its claimed,
//! full inbox makes only the application-data lanes wait.
//!
//! ```text
//!   peer ──App──▶ handled ──▶ inbox full: waits ──(app reads)──▶ queued
//!        ──App──▶ queued behind it on the Application lane
//!        ──DhtControl / Storage──▶ handled, never waiting on the application
//! ```

use std::sync::Arc;

use super::super::transport_with_measure;
use super::super::RecordingMeasure;
use super::claim_full_application_inbox;
use super::local_wire;
use super::next_custom_data;
use super::next_tapped;
use super::next_tapped_custom;
use super::noop_control_message;
use super::release_full_application_inbox;
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
use crate::message::Message;
use crate::message::SyncEntriesWithSuccessor;
use crate::swarm::callback::InnerSwarmCallback;
use crate::tests::live_entry;

/// The ring positions of the entries the storage frame of these tests places locally.
const STORED_POSITIONS: [u32; 2] = [41, 42];

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

/// Law (only the application lanes wait): with the inbox claimed, full and never read, an
/// application frame is handled and then waits for room, and the next application frame from
/// the same peer waits behind it; a control frame and a storage frame from that peer are still
/// handled, and the storage frame's entries reach the DHT.
#[tokio::test]
async fn test_unread_full_inbox_holds_only_the_application_lane() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let local = transport.dht.did;
    let _inbox = claim_full_application_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(InnerSwarmCallback::new(Arc::clone(&transport)));
    let cid = peer.to_string();

    let waiting = spawn_inbound_delivery(
        Arc::clone(&callback),
        cid.clone(),
        local_wire(Message::custom(b"waits-for-room")?, &session, local)?,
    );
    assert_eq!(
        next_tapped_custom(&mut tap).await?,
        b"waits-for-room".to_vec()
    );
    let queued = spawn_inbound_delivery(
        Arc::clone(&callback),
        cid.clone(),
        local_wire(Message::custom(b"queued-behind")?, &session, local)?,
    );
    callback
        .await_inbound_admitted_count_for_test(|admitted| admitted == 2)
        .await;

    callback
        .on_admitted_message_for_test(
            &cid,
            &local_wire(noop_control_message(local), &session, local)?,
        )
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_admitted_message_for_test(&cid, &local_wire(storage_message(local), &session, local)?)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(matches!(
        next_tapped(&mut tap).await?,
        Message::FindSuccessorReport(_)
    ));
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
    // The queued application frame never entered the logical stage.
    assert!(tap.try_recv().is_err());
    assert!(!waiting.is_finished());
    assert!(!queued.is_finished());
    Ok(())
}

/// Law (reading releases): reading the inbox makes room, so the waiting application delivery
/// completes, the one queued behind it follows, and both reach the application in arrival order.
#[tokio::test]
async fn test_reading_the_inbox_releases_the_waiting_application_delivery() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let session = DelegateeKey::new_with_seckey(&peer_key)?;
    let local = transport.dht.did;
    let inbox = claim_full_application_inbox(&transport)?;
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(InnerSwarmCallback::new(Arc::clone(&transport)));
    let cid = peer.to_string();

    let waiting = spawn_inbound_delivery(
        Arc::clone(&callback),
        cid.clone(),
        local_wire(Message::custom(b"waits-for-room")?, &session, local)?,
    );
    next_tapped(&mut tap).await?;
    let queued = spawn_inbound_delivery(
        Arc::clone(&callback),
        cid,
        local_wire(Message::custom(b"queued-behind")?, &session, local)?,
    );
    callback
        .await_inbound_admitted_count_for_test(|admitted| admitted == 2)
        .await;

    release_full_application_inbox(&inbox)?;
    waiting
        .await
        .map_err(|_| Error::InvalidMessage("waiting delivery task panicked".to_string()))??;
    queued
        .await
        .map_err(|_| Error::InvalidMessage("queued delivery task panicked".to_string()))??;
    assert_eq!(next_custom_data(&inbox).await?, b"waits-for-room".to_vec());
    assert_eq!(next_custom_data(&inbox).await?, b"queued-behind".to_vec());
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    Ok(())
}
