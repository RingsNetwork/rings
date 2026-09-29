//! The law of whole transitions (#914 review round 2, M1): cancelling the caller of a
//! reservation or an admission mid-persist abandons only the wait. The transition still
//! persists its record and updates its table before the next transition of the stream runs, so
//! the next one cannot compute from a table without it, nor be overtaken on disk by it.

use std::num::NonZeroU64;
use std::sync::Arc;

use tokio::sync::Notify;

use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;
use crate::storage::MemStorage;
use crate::storage::ScannedRecord;

/// The digest of the transaction a test admits, one per `value`.
fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

/// The one stream the tests drive.
fn stream() -> StreamKey {
    StreamKey::new(
        7,
        Did::from(1_u32),
        Did::from(99_u32),
        MessageCategory::Application,
    )
}

/// A memory store whose every `put` announces itself and then waits at a gate the test opens,
/// so a write can be held mid-persist deterministically.
#[derive(Default)]
struct GatedStorage {
    /// The stored records.
    inner: MemStorage<ReplayRecord>,
    /// Signalled once per `put` that reached the gate.
    entered: Notify,
    /// Opened once per `put` the test lets through.
    release: Notify,
}

#[async_trait::async_trait]
impl KvStorageInterface<ReplayRecord> for GatedStorage {
    async fn get(&self, key: &str) -> Result<Option<ReplayRecord>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &ReplayRecord) -> Result<()> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.put(key, value).await
    }

    async fn get_all(&self) -> Result<Vec<(String, ReplayRecord)>> {
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

#[async_trait::async_trait]
impl KvStorageScan<ReplayRecord> for GatedStorage {
    async fn scan(&self) -> Result<Vec<ScannedRecord<ReplayRecord>>> {
        self.inner.scan().await
    }

    fn record_name(&self, key: &str) -> String {
        self.inner.record_name(key)
    }
}

/// Admit `sequence` of the stream through the detached path, as the transport does.
async fn admit(
    replay: &Arc<TransactionReplay>,
    sequence: u64,
    value: u8,
) -> Result<SequenceVerdict> {
    replay
        .admit_with_quota(
            stream(),
            sequence,
            digest(value),
            MessageCategory::Application.into(),
            0,
        )
        .await
}

/// Cancel `task` and confirm it ended by cancellation.
async fn cancel<T>(task: tokio::task::JoinHandle<T>) {
    task.abort();
    assert!(task.await.is_err_and(|error| error.is_cancelled()));
}

/// An admission cancelled mid-persist still lands and updates the window: the next admission
/// of the stream waits for it, computes from a window holding it, and its record holds both,
/// in this run and after a restart.
#[tokio::test]
async fn test_a_cancelled_admission_completes_before_the_next_one() -> Result<()> {
    let storage = Arc::new(GatedStorage::default());
    let replay = TransactionReplay::new(Box::new(Arc::clone(&storage)));

    let cancelled = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { admit(&replay, 5, 5).await }
    });
    storage.entered.notified().await;
    cancel(cancelled).await;
    storage.release.notify_one();

    let next = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { admit(&replay, 6, 6).await }
    });
    storage.entered.notified().await;
    storage.release.notify_one();
    assert_eq!(
        next.await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        SequenceVerdict::Advance
    );
    assert!(matches!(
        replay.admit(stream(), 5, digest(5)).await,
        Err(Error::TransactionReplay { .. })
    ));

    let restarted = TransactionReplay::new(Box::new(Arc::clone(&storage)));
    for (sequence, value) in [(5, 5), (6, 6)] {
        assert!(matches!(
            restarted.admit(stream(), sequence, digest(value)).await,
            Err(Error::TransactionReplay { .. })
        ));
    }
    Ok(())
}

/// A reservation cancelled mid-persist still lands and advances the allocator: the next
/// reservation continues after it, so no sequence is handed out twice, now or after a restart.
#[tokio::test]
async fn test_a_cancelled_reservation_completes_before_the_next_one() -> Result<()> {
    let storage = Arc::new(GatedStorage::default());
    let replay = TransactionReplay::new(Box::new(Arc::clone(&storage)));

    let cancelled = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { replay.reserve(stream(), NonZeroU64::MIN).await }
    });
    storage.entered.notified().await;
    cancel(cancelled).await;
    storage.release.notify_one();

    let next = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { replay.reserve(stream(), NonZeroU64::MIN).await }
    });
    storage.entered.notified().await;
    storage.release.notify_one();
    assert_eq!(
        next.await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        1..=1
    );

    let restarted = TransactionReplay::new(Box::new(Arc::clone(&storage)));
    let resumed = tokio::spawn(async move { restarted.reserve(stream(), NonZeroU64::MIN).await });
    storage.entered.notified().await;
    storage.release.notify_one();
    assert_eq!(
        resumed
            .await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        2..=2
    );
    Ok(())
}
