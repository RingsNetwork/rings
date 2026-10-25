use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::test_storage::Hooked;
use super::test_storage::StorageHooks;
use super::*;
use crate::message::MessageCategory;
use crate::message::OriginQuotaInstant;
use crate::message::OriginQuotaLaneConfig;
use crate::storage::MemStorage;
use crate::utils::get_epoch_ms;

const ONE_SECOND: u128 = 1_000_000_000;

fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

fn stream(origin: u32) -> StreamKey {
    StreamKey::new(
        7,
        Did::from(origin),
        Did::from(99_u32),
        MessageCategory::Application,
    )
}

fn quota_config(message_burst: u64) -> OriginQuotaConfig {
    quota_config_with_capacity(message_burst, 8)
}

fn quota_config_with_capacity(message_burst: u64, max_records: usize) -> OriginQuotaConfig {
    let lane = OriginQuotaLaneConfig::new(1, message_burst, 1_000, 1_000, max_records)
        .expect("test quota configuration is valid");
    OriginQuotaConfig::new(lane, lane, lane, lane)
}

fn runtime(message_burst: u64) -> Arc<TransactionReplay> {
    Arc::new(TransactionReplay::with_quota_for_test(
        Some(Box::new(MemStorage::<ReplayRecord>::new())),
        quota_config(message_burst),
    ))
}

/// An application admission of `key` at `sequence` costing one byte, introduced by this node.
fn costing_one(key: StreamKey, sequence: u64, digest: TransactionDigest) -> Admission {
    Admission {
        byte_cost: 1,
        ..Admission::for_test(key, sequence, digest, get_epoch_ms())
    }
}

async fn admit_at(
    runtime: &Arc<TransactionReplay>,
    key: StreamKey,
    sequence: u64,
    digest: TransactionDigest,
    now: u128,
) -> Result<SequenceVerdict> {
    runtime
        .admit_at(costing_one(key, sequence, digest), move || {
            ReplayInstant::quota_at(OriginQuotaInstant::from_nanos(now))
        })
        .await
}

#[tokio::test]
async fn replay_fork_and_stale_verdicts_do_not_consume_quota() -> Result<()> {
    let replay_runtime = runtime(2);
    let key = stream(1);
    assert_eq!(
        admit_at(&replay_runtime, key, 0, digest(1), 0).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        admit_at(&replay_runtime, key, 0, digest(1), 0).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(
        admit_at(&replay_runtime, key, 1, digest(2), 0).await?,
        SequenceVerdict::Advance
    );

    let fork_runtime = runtime(2);
    assert_eq!(
        admit_at(&fork_runtime, key, 0, digest(1), 0).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        admit_at(&fork_runtime, key, 0, digest(2), 0).await,
        Err(Error::TransactionSequenceFork { .. })
    ));
    assert_eq!(
        admit_at(&fork_runtime, key, 1, digest(3), 0).await?,
        SequenceVerdict::Advance
    );

    let stale_runtime = runtime(2);
    assert_eq!(
        admit_at(&stale_runtime, key, 32, digest(1), 0).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        admit_at(&stale_runtime, key, 0, digest(2), 0).await,
        Err(Error::TransactionSequenceStale { .. })
    ));
    assert_eq!(
        admit_at(&stale_runtime, key, 33, digest(3), 0).await?,
        SequenceVerdict::Advance
    );
    Ok(())
}

#[tokio::test]
async fn quota_rejection_does_not_advance_replay_and_retry_can_commit() -> Result<()> {
    let runtime = runtime(1);
    let key = stream(1);
    assert_eq!(
        admit_at(&runtime, key, 0, digest(1), 0).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        admit_at(&runtime, key, 1, digest(2), 0).await,
        Err(Error::OriginQuota(
            crate::message::OriginQuotaError::MessageRateExhausted { .. }
        ))
    ));
    assert_eq!(
        runtime
            .quota_counters()
            .lane(MessageCategory::Application)
            .message_rate_exhausted,
        1
    );
    assert_eq!(
        admit_at(&runtime, key, 1, digest(2), ONE_SECOND).await?,
        SequenceVerdict::Advance
    );
    Ok(())
}

#[tokio::test]
async fn capacity_rejection_preserves_replay_until_an_idle_slot_is_safe() -> Result<()> {
    let runtime = Arc::new(TransactionReplay::with_quota_for_test(
        Some(Box::new(MemStorage::<ReplayRecord>::new())),
        quota_config_with_capacity(1, 1),
    ));
    let first = stream(1);
    let waiting = stream(2);
    assert_eq!(
        admit_at(&runtime, first, 0, digest(1), 0).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        admit_at(&runtime, waiting, 0, digest(2), 0).await,
        Err(Error::OriginQuota(
            crate::message::OriginQuotaError::TableCapacityExhausted {
                lane: MessageCategory::Application,
                capacity: 1,
            }
        ))
    ));
    assert_eq!(
        runtime
            .quota_counters()
            .lane(MessageCategory::Application)
            .capacity_exhausted,
        1
    );
    assert_eq!(
        admit_at(&runtime, waiting, 0, digest(2), ONE_SECOND).await?,
        SequenceVerdict::First
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_duplicates_cross_the_combined_boundary_once() {
    let runtime = runtime(2);
    let key = stream(1);
    let first = admit_at(&runtime, key, 0, digest(1), 0);
    let second = admit_at(&runtime, key, 0, digest(1), 0);
    let (first, second) = futures::join!(first, second);
    let admitted = [&first, &second]
        .iter()
        .filter(|result| result.is_ok())
        .count();
    let replayed = [&first, &second]
        .iter()
        .filter(|result| matches!(result, Err(Error::TransactionReplay { .. })))
        .count();

    assert_eq!(admitted, 1);
    assert_eq!(replayed, 1);
}

/// Hooks that refuse the first write and pass every later one.
struct FailFirstPut(AtomicBool);

#[async_trait::async_trait]
impl StorageHooks for FailFirstPut {
    /// Runs before a `put`.
    async fn before_put(&self, _key: &str) -> Result<()> {
        match self.0.swap(false, Ordering::AcqRel) {
            true => Err(Error::ServiceIOError(std::io::Error::other(
                "store unavailable",
            ))),
            false => Ok(()),
        }
    }
}

#[tokio::test]
async fn replay_persistence_failure_rolls_back_provisional_quota() -> Result<()> {
    let runtime = Arc::new(TransactionReplay::with_quota_for_test(
        Some(Box::new(Hooked::new(FailFirstPut(AtomicBool::new(true))))),
        quota_config(1),
    ));
    let key = stream(1);
    assert!(matches!(
        admit_at(&runtime, key, 0, digest(1), 0).await,
        Err(Error::TransactionReplayPersistence {
            operation: "store",
            ..
        })
    ));
    assert_eq!(
        admit_at(&runtime, key, 0, digest(1), 0).await?,
        SequenceVerdict::First
    );
    Ok(())
}

/// Law (Clock under the lock, #953): an admission reads the quota clock only while it holds
/// the admission lock. Two admissions on one quota key wait on the held lock; neither reads
/// its clock until the lock is released, and the one that locks first reads first and is the
/// stream's `First`. An instant fixed before the lock could hand the later locker an instant
/// older than the one recorded and fail with `MonotonicTimeRegressed`; read under the lock,
/// the instants follow the lock order and both admissions commit.
#[tokio::test]
async fn test_quota_clock_is_read_under_the_admission_lock() -> Result<()> {
    let runtime = runtime(4);
    let key = stream(1);
    let readings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let clock = |label: &'static str| {
        let readings = std::sync::Arc::clone(&readings);
        move || {
            let mut readings = readings.lock().expect("test clock lock");
            let tick = readings.len() as u128;
            readings.push(label);
            ReplayInstant::quota_at(OriginQuotaInstant::from_nanos(tick * ONE_SECOND))
        }
    };

    let held = runtime.state.lock().await;
    let mut first = std::pin::pin!(runtime.admit_at(costing_one(key, 0, digest(1)), clock("A")));
    let mut second = std::pin::pin!(runtime.admit_at(costing_one(key, 1, digest(2)), clock("B")));
    assert!(futures::poll!(first.as_mut()).is_pending());
    assert!(futures::poll!(second.as_mut()).is_pending());
    assert!(readings.lock().expect("test clock lock").is_empty());
    drop(held);

    let (first, second) = (first.await?, second.await?);
    let read = readings.lock().expect("test clock lock").clone();
    let expected = match (first, second) {
        (SequenceVerdict::First, SequenceVerdict::Advance) => ["A", "B"],
        (SequenceVerdict::Late, SequenceVerdict::First) => ["B", "A"],
        verdicts => panic!("unexpected verdicts {verdicts:?}"),
    };
    assert_eq!(read, expected);
    Ok(())
}
