//! Replay loads that fail: a store that cannot be read or written.

use super::test_storage::Hooked;
use super::test_storage::StorageHooks;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;

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
        Err(Error::ServiceIOError(std::io::Error::other(
            "store unavailable",
        )))
    }

    /// Runs before a whole-store read.
    async fn before_scan(&self) -> Result<()> {
        Err(Error::ServiceIOError(std::io::Error::other(
            "store unavailable",
        )))
    }
}

/// Hooks of a store that loads but refuses every write.
struct WritesRefused;

#[async_trait::async_trait]
impl StorageHooks for WritesRefused {
    /// Runs before a `put`.
    async fn before_put(&self, _key: &str) -> Result<()> {
        Err(Error::ServiceIOError(std::io::Error::other(
            "store unavailable",
        )))
    }
}

/// A runtime given no replay store refuses every reservation and admission (#1022): it never
/// keeps a window in memory, so no restart can reopen a stream, and nothing is counted as a
/// persistence failure, since there is no store to fail.
#[tokio::test]
async fn test_a_runtime_without_a_store_refuses_every_transition() {
    let runtime = std::sync::Arc::new(TransactionReplay::new_with_quota(
        None,
        crate::message::OriginQuotaConfig::default(),
    ));
    let key = stream(SecretKey::random().address().into());

    assert!(matches!(
        runtime.admit(key, 0, digest(1)).await,
        Err(Error::TransactionReplayStorageUnavailable)
    ));
    assert!(matches!(
        runtime.reserve(key, std::num::NonZeroU64::MIN).await,
        Err(Error::TransactionReplayStorageUnavailable)
    ));
    assert_eq!(runtime.counters().persistence_failure, 0);
}
