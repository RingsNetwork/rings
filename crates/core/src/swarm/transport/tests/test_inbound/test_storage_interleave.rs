use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use async_trait::async_trait;

use super::super::local_wire;
use super::super::unbound_peer;
use super::super::ApplicationRecorder;
use super::super::TestLatch;
use super::super::TransportSpec;
use super::spawn_inbound_delivery;
use super::storage_message;
use crate::dht::entry::Entry;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;
use crate::swarm::transport::inbound_tap::LaneHold;

/// The witnesses of one storage batch interleaved with an application delivery waiting at its
/// held lane.
#[derive(Default)]
struct InterleaveProbe {
    /// Persistence steps the storage batch has taken.
    put_calls: AtomicUsize,
    /// Set once the first entry is persisted.
    first_persisted: TestLatch,
    /// Released by the test to let the first persistence step return.
    release_first_persist: TestLatch,
    /// The application end, where the second step looks for the woken delivery.
    application: OnceLock<ApplicationRecorder>,
    /// The hold on the Application lane, dropped at the end of the first step to wake the
    /// waiting application delivery from within the storage batch.
    lane_hold: Mutex<Option<LaneHold>>,
}

impl InterleaveProbe {
    /// The application end; attached before any frame is submitted.
    fn application(&self) -> Result<&ApplicationRecorder> {
        self.application
            .get()
            .ok_or_else(|| Error::InvalidMessage("application end not attached".to_string()))
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
        if call == 1 && self.probe.application()?.inbounds() == 0 {
            return Err(Error::InvalidMessage(
                "second storage effect ran before the woken application delivery".to_string(),
            ));
        }
        self.inner.put(key, value).await?;
        if call == 0 {
            self.probe.first_persisted.set();
            self.probe.release_first_persist.wait().await;
            // Releasing the lane wakes the waiting application delivery here, before this step
            // returns and the batch yields.
            let lane_hold = self
                .probe
                .lane_hold
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or_else(|| {
                    Error::InvalidMessage("the Application lane is not held".to_string())
                })?;
            drop(lane_hold);
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
/// waiting at its held lane, which the first step releases; the second step checks it was
/// queued.
#[tokio::test]
async fn test_inbound_storage_batch_yields_between_persistence_steps() -> Result<()> {
    let probe = Arc::new(InterleaveProbe::default());
    let transport = Arc::new(
        TransportSpec {
            storage: Box::new(InterleaveStorage {
                inner: MemStorage::new(),
                probe: Arc::clone(&probe),
            }),
            ..TransportSpec::default()
        }
        .build()?,
    );
    let (application, lane_hold) =
        ApplicationRecorder::attach_holding_application_lane(&transport)?;
    *probe
        .lane_hold
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(lane_hold);
    if probe.application.set(application).is_err() {
        return Err(Error::InvalidMessage(
            "application end attached twice".to_string(),
        ));
    }
    let peer = unbound_peer(&transport, &SecretKey::random())?;
    let callback = Arc::clone(&peer.callback);
    let cid = peer.peer.to_string();

    let storage = local_wire(
        storage_message(transport.dht.did, [31, 32]),
        &peer.session,
        transport.dht.did,
    )?;
    let storage_delivery = spawn_inbound_delivery(Arc::clone(&callback), cid.clone(), storage);
    probe.first_persisted.wait().await;

    let application =
        peer.custom_message_wire(&transport, b"waits-at-the-held-application-lane")?;
    let application_delivery = spawn_inbound_delivery(callback, cid, application);
    // The storage frame entered the logical stage first, then the application frame, which
    // waits at its held lane once it is observed.
    for _ in 0..2 {
        probe.application()?.next_tapped().await?;
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
