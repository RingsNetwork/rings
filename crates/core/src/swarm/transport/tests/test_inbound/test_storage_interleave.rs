use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;

use super::super::RecordingMeasure;
use super::super::TestLatch;
use super::local_wire;
use super::spawn_inbound_delivery;
use crate::chunk::ReassemblyLimits;
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
use crate::message::MessagePayload;
use crate::message::SyncEntriesWithSuccessor;
use crate::session::SessionSk;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::transport::SwarmTransport;
use crate::swarm::transport::SwarmTransportSettings;
use crate::swarm::transport::SwarmWebrtcConfig;

#[derive(Default)]
struct InterleaveProbe {
    put_calls: AtomicUsize,
    first_persisted: TestLatch,
    release_first_persist: TestLatch,
    control_waiting: TestLatch,
    control_may_progress: TestLatch,
    control_progressed: TestLatch,
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
        if call == 1 && !self.probe.control_progressed.is_set() {
            return Err(Error::InvalidMessage(
                "second storage effect ran before queued control traffic".to_string(),
            ));
        }
        self.inner.put(key, value).await?;
        if call == 0 {
            self.probe.first_persisted.set();
            self.probe.release_first_persist.wait().await;
            self.probe.control_may_progress.set();
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

struct InterleaveCallback {
    probe: Arc<InterleaveProbe>,
}

#[async_trait]
impl SwarmCallback for InterleaveCallback {
    async fn on_validate(
        &self,
        payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        if matches!(
            payload.transaction.data::<Message>()?,
            Message::PeerLivenessReport(_)
        ) {
            self.probe.control_waiting.set();
            self.probe.control_may_progress.wait().await;
            self.probe.control_progressed.set();
        }
        Ok(())
    }
}

#[tokio::test]
async fn test_inbound_storage_batch_yields_to_control_between_persistence_steps() -> Result<()> {
    let probe = Arc::new(InterleaveProbe::default());
    let local_session = SessionSk::new_with_seckey(&SecretKey::random())?;
    let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
        local_session.account_did(),
        3,
        Box::new(InterleaveStorage {
            inner: MemStorage::new(),
            probe: Arc::clone(&probe),
        }),
        DEFAULT_FINGER_TABLE_SIZE,
    ));
    let transport = Arc::new(SwarmTransport::new(
        0,
        SwarmWebrtcConfig::new("".to_string(), None, None),
        local_session,
        dht,
        Some(Arc::new(RecordingMeasure::default())),
        SwarmTransportSettings::new(
            1,
            VirtualNodeConfig::disabled(),
            ReassemblyLimits::production(),
        ),
    ));
    let callback = Arc::new(InnerSwarmCallback::new(
        Arc::clone(&transport),
        Arc::new(InterleaveCallback {
            probe: Arc::clone(&probe),
        }),
    ));
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let cid = peer.to_string();

    let entries = [31_u32, 32_u32]
        .into_iter()
        .map(|did| {
            let entry = crate::tests::live_entry(Did::from(did), Vec::new(), EntryKind::Data);
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

    let control = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
    )?;
    let control_delivery = spawn_inbound_delivery(callback, cid, control);
    probe.control_waiting.wait().await;
    probe.release_first_persist.set();

    storage_delivery
        .await
        .map_err(|_| Error::InvalidMessage("storage mailbox task panicked".to_string()))??;
    control_delivery
        .await
        .map_err(|_| Error::InvalidMessage("control mailbox task panicked".to_string()))??;

    assert_eq!(probe.put_calls.load(Ordering::SeqCst), 2);
    assert!(probe.control_progressed.is_set());
    Ok(())
}
