use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;
use crate::message::OriginQuotaLaneConfig;
use crate::storage::MemStorage;

const ONE_SECOND: u128 = 1_000_000_000;

fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

fn stream(origin: u32) -> StreamKey {
    StreamKey::new(7, Did::from(origin), Did::from(99_u32))
}

fn quota_config(message_burst: u64) -> OriginQuotaConfig {
    quota_config_with_capacity(message_burst, 8)
}

fn quota_config_with_capacity(message_burst: u64, max_records: usize) -> OriginQuotaConfig {
    let lane = OriginQuotaLaneConfig::new(1, message_burst, 1_000, 1_000, max_records)
        .expect("test quota configuration is valid");
    OriginQuotaConfig::new(lane, lane, lane, lane)
}

fn runtime(message_burst: u64) -> TransactionReplay {
    TransactionReplay::new_with_quota(
        Box::new(MemStorage::<ReplaySnapshot>::new()),
        quota_config(message_burst),
    )
}

async fn admit_at(
    runtime: &TransactionReplay,
    key: StreamKey,
    sequence: u64,
    digest: TransactionDigest,
    now: u128,
) -> Result<SequenceVerdict> {
    runtime
        .admit_at(
            key,
            sequence,
            digest,
            OriginQuotaLane::Application,
            1,
            OriginQuotaInstant::from_nanos(now),
        )
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
            .lane(OriginQuotaLane::Application)
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
    let runtime = TransactionReplay::new_with_quota(
        Box::new(MemStorage::<ReplaySnapshot>::new()),
        quota_config_with_capacity(1, 1),
    );
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
                lane: OriginQuotaLane::Application,
                capacity: 1,
            }
        ))
    ));
    assert_eq!(
        runtime
            .quota_counters()
            .lane(OriginQuotaLane::Application)
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

struct FailFirstPutStorage {
    inner: MemStorage<ReplaySnapshot>,
    fail_next: AtomicBool,
}

impl FailFirstPutStorage {
    fn new() -> Self {
        Self {
            inner: MemStorage::new(),
            fail_next: AtomicBool::new(true),
        }
    }
}

#[async_trait::async_trait]
impl KvStorageInterface<ReplaySnapshot> for FailFirstPutStorage {
    async fn get(&self, key: &str) -> Result<Option<ReplaySnapshot>> {
        self.inner.get(key).await
    }

    async fn put(&self, key: &str, value: &ReplaySnapshot) -> Result<()> {
        if self.fail_next.swap(false, Ordering::AcqRel) {
            return Err(Error::InvalidTransport);
        }
        self.inner.put(key, value).await
    }

    async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
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

#[tokio::test]
async fn replay_persistence_failure_rolls_back_provisional_quota() -> Result<()> {
    let runtime =
        TransactionReplay::new_with_quota(Box::new(FailFirstPutStorage::new()), quota_config(1));
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
