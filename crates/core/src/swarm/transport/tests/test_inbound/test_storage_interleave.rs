use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

use super::super::RecordingMeasure;
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
    first_persisted: AtomicBool,
    first_persisted_notify: Notify,
    release_first_persist: AtomicBool,
    release_first_persist_notify: Notify,
    control_waiting: AtomicBool,
    control_waiting_notify: Notify,
    control_may_progress: AtomicBool,
    control_may_progress_notify: Notify,
    control_progressed: AtomicBool,
}

impl InterleaveProbe {
    async fn wait_for_control_waiting(&self) {
        loop {
            let notified = self.control_waiting_notify.notified();
            if self.control_waiting.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_first_persist(&self) {
        loop {
            let notified = self.first_persisted_notify.notified();
            if self.first_persisted.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_first_persist_release(&self) {
        loop {
            let notified = self.release_first_persist_notify.notified();
            if self.release_first_persist.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn release_first_persist(&self) {
        self.release_first_persist.store(true, Ordering::SeqCst);
        self.release_first_persist_notify.notify_waiters();
    }

    async fn wait_for_control_progress_permission(&self) {
        loop {
            let notified = self.control_may_progress_notify.notified();
            if self.control_may_progress.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
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
        if call == 1 && !self.probe.control_progressed.load(Ordering::SeqCst) {
            return Err(Error::InvalidMessage(
                "second storage effect ran before queued control traffic".to_string(),
            ));
        }
        self.inner.put(key, value).await?;
        if call == 0 {
            self.probe.first_persisted.store(true, Ordering::SeqCst);
            self.probe.first_persisted_notify.notify_waiters();
            self.probe.wait_for_first_persist_release().await;
            self.probe
                .control_may_progress
                .store(true, Ordering::SeqCst);
            self.probe.control_may_progress_notify.notify_waiters();
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
            self.probe.control_waiting.store(true, Ordering::SeqCst);
            self.probe.control_waiting_notify.notify_waiters();
            self.probe.wait_for_control_progress_permission().await;
            self.probe.control_progressed.store(true, Ordering::SeqCst);
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
            let entry = Entry::new(Did::from(did), Vec::new(), EntryKind::Data);
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
    tokio::time::timeout(Duration::from_secs(1), probe.wait_for_first_persist())
        .await
        .map_err(|_| Error::InvalidMessage("first storage persistence timed out".to_string()))?;

    let control = local_wire(
        Message::PeerLivenessReport(crate::message::PeerLivenessReport { sent_at_ms: 1 }),
        &peer_session,
        transport.dht.did,
    )?;
    let control_delivery = spawn_inbound_delivery(callback, cid, control);
    tokio::time::timeout(Duration::from_secs(1), probe.wait_for_control_waiting())
        .await
        .map_err(|_| Error::InvalidMessage("control admission timed out".to_string()))?;
    probe.release_first_persist();

    tokio::time::timeout(Duration::from_secs(1), async {
        storage_delivery
            .await
            .map_err(|_| Error::InvalidMessage("storage mailbox task panicked".to_string()))??;
        control_delivery
            .await
            .map_err(|_| Error::InvalidMessage("control mailbox task panicked".to_string()))??;
        Ok::<(), Error>(())
    })
    .await
    .map_err(|_| Error::InvalidMessage("storage/control interleave timed out".to_string()))??;

    assert_eq!(probe.put_calls.load(Ordering::SeqCst), 2);
    assert!(probe.control_progressed.load(Ordering::SeqCst));
    Ok(())
}
