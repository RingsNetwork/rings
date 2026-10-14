//! Replay loads that fail: a store that cannot be read or written, and the cutover of the
//! shared-stream snapshot of the key used before #898.

use std::sync::atomic::Ordering;

use super::store::record_key;
use super::store::ReplayTable;
use super::test_storage::Hooked;
use super::test_storage::StorageHooks;
use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use super::SHARED_STREAM_SNAPSHOT_KEY;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::storage::KvStorageInterface;

/// The digest of the transaction a test admits, one per `value`.
fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

/// The application stream from a fresh origin to `destination`.
fn stream(destination: Did) -> StreamKey {
    StreamKey::new(
        7,
        SecretKey::random().address().into(),
        destination,
        MessageCategory::Application,
    )
}

/// A store that cannot be read fails the load closed: the transition is refused and the failed
/// read is counted.
#[tokio::test]
async fn test_load_failure_fails_closed_and_is_counted() {
    let runtime = TransactionReplay::new_shared(Box::new(Hooked::new(Unavailable)));
    let key = stream(SecretKey::random().address().into());

    assert!(matches!(
        runtime.admit(key, 0, digest(1)).await,
        Err(Error::TransactionReplayPersistence {
            operation: "load",
            ..
        })
    ));
    assert_eq!(runtime.counters().persistence_failure, 1);
}

/// A store that refuses the record write fails the admission closed before it is admitted, and
/// the failed write is counted.
#[tokio::test]
async fn test_store_failure_fails_closed_before_admission_and_is_counted() {
    let runtime = TransactionReplay::new_shared(Box::new(Hooked::new(WritesRefused)));
    let key = stream(SecretKey::random().address().into());

    assert!(matches!(
        runtime.admit(key, 0, digest(1)).await,
        Err(Error::TransactionReplayPersistence {
            operation: "store",
            ..
        })
    ));
    assert_eq!(runtime.counters().persistence_failure, 1);
}

/// Hooks of a store that cannot be read or written, modelling a store that cannot load.
/// Removal (and `clear`, `count`) still succeeds: the shared-snapshot retirement must not
/// be what fails.
struct Unavailable;

#[async_trait::async_trait]
impl StorageHooks for Unavailable {
    /// Runs before a `put`.
    async fn before_put(&self, _key: &str) -> Result<()> {
        Err(Error::InvalidTransport)
    }

    /// Runs before a whole-store read.
    async fn before_scan(&self) -> Result<()> {
        Err(Error::InvalidTransport)
    }
}

/// Hooks of a store that loads but refuses every write.
struct WritesRefused;

#[async_trait::async_trait]
impl StorageHooks for WritesRefused {
    /// Runs before a `put`.
    async fn before_put(&self, _key: &str) -> Result<()> {
        Err(Error::InvalidTransport)
    }
}

/// Hooks of a store that still holds a shared-stream snapshot under the key used before
/// #898, counting removals. The snapshot's bytes decode as no stream: had a load decoded them,
/// the snapshot would be an unrestorable record, not the snapshot, and would never be removed,
/// so the removal count is the witness that the cutover never decodes it.
struct Cutover {
    /// Whether removing a record fails.
    fail_remove: bool,
    /// Removals attempted.
    removals: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl StorageHooks for Cutover {
    /// Runs before a `remove`.
    async fn before_remove(&self, _key: &str) -> Result<()> {
        self.removals.fetch_add(1, Ordering::SeqCst);
        match self.fail_remove {
            true => Err(Error::InvalidTransport),
            false => Ok(()),
        }
    }
}

/// A store holding a snapshot under the shared-stream key, whose removals fail iff
/// `fail_remove`.
async fn holding_a_shared_stream_snapshot(
    fail_remove: bool,
) -> Result<std::sync::Arc<Hooked<Cutover>>> {
    let storage = Hooked::new(Cutover {
        fail_remove,
        removals: std::sync::atomic::AtomicUsize::new(0),
    });
    storage
        .inner
        .put(SHARED_STREAM_SNAPSHOT_KEY, &ReplayRecord(vec![0xff; 3]))
        .await?;
    Ok(std::sync::Arc::new(storage))
}

/// Cutover of #898: the first load deletes the shared-stream snapshot without decoding it,
/// and admission proceeds on fresh per-class streams. A restart after it never deletes
/// again: the former snapshot is gone.
#[tokio::test]
async fn test_first_load_deletes_the_shared_stream_snapshot_unread() -> Result<()> {
    let storage = holding_a_shared_stream_snapshot(false).await?;
    let runtime = TransactionReplay::new_shared(Box::new(storage.clone()));
    let key = stream(SecretKey::random().address().into());

    assert_eq!(
        runtime.admit(key, 0, digest(1)).await?,
        SequenceVerdict::First
    );
    assert!(storage
        .inner
        .get(SHARED_STREAM_SNAPSHOT_KEY)
        .await?
        .is_none());
    assert!(storage
        .inner
        .get(record_key(ReplayTable::Receiver, &key)?.as_str())
        .await?
        .is_some());
    assert_eq!(runtime.counters().persistence_failure, 0);

    let restarted = TransactionReplay::new_shared(Box::new(storage.clone()));
    assert_eq!(
        restarted.admit(key, 1, digest(2)).await?,
        SequenceVerdict::Advance
    );
    assert_eq!(storage.hooks.removals.load(Ordering::SeqCst), 1);
    Ok(())
}

/// A failed deletion of the shared-stream snapshot is counted and admission continues: the
/// former key is never decoded, so it is left inert until a later load deletes it.
#[tokio::test]
async fn test_failed_shared_stream_deletion_is_counted_and_admission_continues() -> Result<()> {
    let storage = holding_a_shared_stream_snapshot(true).await?;
    let runtime = TransactionReplay::new_shared(Box::new(storage.clone()));
    let key = stream(SecretKey::random().address().into());

    assert_eq!(
        runtime.admit(key, 0, digest(1)).await?,
        SequenceVerdict::First
    );
    assert_eq!(runtime.counters().persistence_failure, 1);
    assert!(storage
        .inner
        .get(SHARED_STREAM_SNAPSHOT_KEY)
        .await?
        .is_some());

    let restarted = TransactionReplay::new_shared(Box::new(storage.clone()));
    assert_eq!(
        restarted.admit(key, 1, digest(2)).await?,
        SequenceVerdict::Advance
    );
    assert_eq!(restarted.counters().persistence_failure, 1);
    assert_eq!(storage.hooks.removals.load(Ordering::SeqCst), 2);
    Ok(())
}
