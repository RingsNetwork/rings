use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use futures::StreamExt;

use super::super::RecordingMeasure;
use super::super::TestLatch;
use super::claim_full_application_inbox;
use super::local_wire;
use super::spawn_inbound_delivery;
use crate::chunk::ReassemblyLimits;
use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::dht::VirtualNodeConfig;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::SyncEntriesWithSuccessor;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::swarm::application::ApplicationDelivery;
use crate::swarm::application::ApplicationInbox;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::observer::NoopSwarmObserver;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::SwarmTransportParts;
use crate::swarm::transport::SwarmTransportSettings;
use crate::swarm::transport::SwarmWebrtcConfig;
use crate::tests::live_entry;

/// The witnesses of one storage batch interleaved with a waiting application delivery.
#[derive(Default)]
struct InterleaveProbe {
    /// Persistence steps the storage batch has taken.
    put_calls: AtomicUsize,
    /// Set once the first entry is persisted.
    first_persisted: TestLatch,
    /// Released by the test to let the first persistence step return.
    release_first_persist: TestLatch,
    /// The claimed, full application inbox: reading it at the end of the first step makes room
    /// for the waiting application delivery.
    inbox: OnceLock<ApplicationInbox>,
}

impl InterleaveProbe {
    /// The application inbox; claimed before any frame is submitted.
    fn inbox(&self) -> Result<&ApplicationInbox> {
        self.inbox
            .get()
            .ok_or_else(|| Error::InvalidMessage("application inbox not claimed".to_string()))
    }
}

struct InterleaveStorage {
    inner: MemStorage<Entry>,
    probe: Arc<InterleaveProbe>,
}

#[async_trait]
impl KvStorageInterface<Entry> for InterleaveStorage {
    async fn get(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &Entry) -> Result<()> {
        let call = self.probe.put_calls.fetch_add(1, Ordering::SeqCst);
        if call == 1
            && !matches!(
                self.probe.inbox()?.try_next(),
                Some(ApplicationDelivery::Message(_))
            )
        {
            return Err(Error::InvalidMessage(
                "second storage effect ran before the woken application delivery".to_string(),
            ));
        }
        self.inner.put(key, value).await?;
        if call == 0 {
            self.probe.first_persisted.set();
            self.probe.release_first_persist.wait().await;
            // Reading the filler makes room: the waiting application delivery is woken here,
            // before this step returns and the batch yields.
            if self.probe.inbox()?.try_next().is_none() {
                return Err(Error::InvalidMessage(
                    "the inbox filler is missing".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn get_all(&self) -> Result<Vec<(String, Entry)>> {
        self.inner.get_all().await
    }

    async fn remove(&self, key: &str) -> Result<()> {
        self.inner.remove(key).await
    }

    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }

    async fn count(&self) -> Result<u32> {
        self.inner.count().await
    }
}

/// A storage batch yields between persistence steps, so work of another lane that became ready
/// during one step runs before the next. The other lane's work is an application delivery
/// waiting for inbox room, which the first step makes; the second step checks it was queued.
#[tokio::test]
async fn test_inbound_storage_batch_yields_between_persistence_steps() -> Result<()> {
    let probe = Arc::new(InterleaveProbe::default());
    let local_session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
        local_session.delegator_did(),
        3,
        Box::new(InterleaveStorage {
            inner: MemStorage::new(),
            probe: Arc::clone(&probe),
        }),
        DEFAULT_FINGER_TABLE_SIZE,
    ));
    let transport = Arc::new(SwarmTransport::new(SwarmTransportParts {
        network_id: 0,
        webrtc: SwarmWebrtcConfig::new("".to_string(), None, None),
        delegatee_key: local_session,
        dht,
        measure: Some(Arc::new(RecordingMeasure::default())),
        transaction_replay: crate::message::TransactionReplay::new_shared(Box::new(
            crate::storage::MemStorage::new(),
        )),
        settings: SwarmTransportSettings::new(
            1,
            VirtualNodeConfig::disabled(),
            ReassemblyLimits::production(),
        ),
        observer: Arc::new(NoopSwarmObserver),
    }));
    if probe
        .inbox
        .set(claim_full_application_inbox(&transport)?)
        .is_err()
    {
        return Err(Error::InvalidMessage(
            "application inbox claimed twice".to_string(),
        ));
    }
    let mut tap = transport.inbound_tap_for_test().subscribe();
    let callback = Arc::new(InnerSwarmCallback::new(Arc::clone(&transport)));
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = DelegateeKey::new_with_seckey(&peer_key)?;
    let cid = peer.to_string();

    let entries = [31_u32, 32_u32]
        .into_iter()
        .map(|did| {
            let entry = live_entry(Did::from(did), Vec::new(), EntryKind::Data);
            PlacedEntry::new(entry.did, entry)
        })
        .collect();
    let storage = local_wire(
        Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::AdditiveRepair,
            destination: StorageSyncDestination::PhysicalOwner(transport.dht.did),
            data: entries,
        }),
        &peer_session,
        transport.dht.did,
    )?;
    let storage_delivery = spawn_inbound_delivery(Arc::clone(&callback), cid.clone(), storage);
    probe.first_persisted.wait().await;

    let application = local_wire(
        Message::custom(b"waits-for-inbox-room")?,
        &peer_session,
        transport.dht.did,
    )?;
    let application_delivery = spawn_inbound_delivery(callback, cid, application);
    // The storage frame entered the logical stage first, then the application frame. The
    // latter's handling has no suspension point before the inbox, so once it is observed it
    // waits for room.
    for _ in 0..2 {
        tap.next()
            .await
            .ok_or_else(|| Error::InvalidMessage("inbound tap closed".to_string()))?;
    }
    assert!(!application_delivery.is_finished());
    probe.release_first_persist.set();

    storage_delivery
        .await
        .map_err(|_| Error::InvalidMessage("storage mailbox task panicked".to_string()))??;
    application_delivery
        .await
        .map_err(|_| Error::InvalidMessage("application mailbox task panicked".to_string()))??;

    assert_eq!(probe.put_calls.load(Ordering::SeqCst), 2);
    Ok(())
}
