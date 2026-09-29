//! The law of whole transitions of `TransactionReplay` (#909, #910): cancelling the caller of a
//! reservation or an admission mid-persist abandons only the wait. The transition still
//! persists its record and updates its table before the next transition of the stream runs, so
//! the next one cannot compute from a table without it, nor be overtaken on disk by it.

use std::num::NonZeroU64;
use std::sync::Arc;

use tokio::sync::Notify;

use super::test_storage::Hooked;
use super::test_storage::StorageHooks;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;

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

/// Hooks that make every `put` announce itself and then wait at a gate the test opens, so a
/// write can be held mid-persist deterministically.
#[derive(Default)]
struct Gate {
    /// Signalled once per `put` that reached the gate.
    entered: Notify,
    /// Opened once per `put` the test lets through.
    release: Notify,
}

#[async_trait::async_trait]
impl StorageHooks for Gate {
    /// Runs before a `put`.
    async fn before_put(&self) -> Result<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// A memory store whose writes wait at a [`Gate`].
type GatedStorage = Hooked<Gate>;

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
    let storage = Arc::new(GatedStorage::new(Gate::default()));
    let replay = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));

    let cancelled = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { admit(&replay, 5, 5).await }
    });
    storage.hooks.entered.notified().await;
    cancel(cancelled).await;
    storage.hooks.release.notify_one();

    let next = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { admit(&replay, 6, 6).await }
    });
    storage.hooks.entered.notified().await;
    storage.hooks.release.notify_one();
    assert_eq!(
        next.await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        SequenceVerdict::Advance
    );
    assert!(matches!(
        replay.admit(stream(), 5, digest(5)).await,
        Err(Error::TransactionReplay { .. })
    ));

    let restarted = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
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
    let storage = Arc::new(GatedStorage::new(Gate::default()));
    let replay = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));

    let cancelled = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { replay.reserve(stream(), NonZeroU64::MIN).await }
    });
    storage.hooks.entered.notified().await;
    cancel(cancelled).await;
    storage.hooks.release.notify_one();

    let next = tokio::spawn({
        let replay = Arc::clone(&replay);
        async move { replay.reserve(stream(), NonZeroU64::MIN).await }
    });
    storage.hooks.entered.notified().await;
    storage.hooks.release.notify_one();
    assert_eq!(
        next.await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        1..=1
    );

    let restarted = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
    let resumed = tokio::spawn(async move { restarted.reserve(stream(), NonZeroU64::MIN).await });
    storage.hooks.entered.notified().await;
    storage.hooks.release.notify_one();
    assert_eq!(
        resumed
            .await
            .map_err(|_| Error::TransactionReplayStateInvalid)??,
        2..=2
    );
    Ok(())
}
