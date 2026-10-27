//! Stream admission under slot accounting (load-bounds paper, Law 2, #907): a new stream is
//! charged to the slot of the edge that delivered it, refused past that slot's floor and fair
//! claim, and retired once spent, which ends the refusal; the charges survive a restart.

use std::sync::Arc;

use super::Admission;
use super::ReplayInstant;
use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::Principal;
use crate::fair_admission::ShareRefusal;
use crate::fair_admission::Shares;
use crate::fair_admission::TableAccounting;
use crate::message::MessageCategory;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaInstant;
use crate::storage::KvStorageInterface;
use crate::storage::MemStorage;

/// One floor item per slot, one pool item, one local item and one inbox item.
const SMALL: Shares = Shares {
    floor: 1,
    pool: 1,
    local: 1,
    inbox: 1,
    custody: 0,
};

/// The instant every stream of these tests is spent at.
const SPENT_AT_MS: u128 = 1_000;

/// The stream of `origin` to this node, of `class`.
fn stream(origin: u32, class: MessageCategory) -> StreamKey {
    StreamKey::new(7, Did::from(origin), Did::from(99_u32), class)
}

/// A replay runtime over `storage` whose receiver table has [`SMALL`] shares.
fn runtime(storage: &Arc<MemStorage<ReplayRecord>>) -> Arc<TransactionReplay> {
    Arc::new(TransactionReplay::new_with_quota(
        Some(Box::new(Arc::clone(storage))),
        OriginQuotaConfig::default(),
        TableAccounting::new(SMALL),
    ))
}

/// Admit the first transaction of `key`, delivered by `introducer`, at wall clock `now_ms`.
async fn open(
    replay: &Arc<TransactionReplay>,
    key: StreamKey,
    introducer: Principal,
    now_ms: u128,
) -> Result<SequenceVerdict> {
    let admission = Admission {
        introducer,
        spent_at_ms: SPENT_AT_MS,
        ..Admission::for_test(key, 0, TransactionDigest::new([1; 32]), now_ms)
    };
    replay
        .admit_at(admission, move || ReplayInstant {
            quota: OriginQuotaInstant::ZERO,
            wall_ms: now_ms,
        })
        .await
}

/// Whether `result` is the refusal of a new stream for a full pool.
fn pool_full(result: &Result<SequenceVerdict>) -> bool {
    matches!(
        result,
        Err(Error::TransactionReplayStreamCapacityExceeded {
            refusal: ShareRefusal::PoolFull { .. },
            ..
        })
    )
}

/// A slot that holds its floor and the pool is refused a new stream of a free origin; another
/// slot still opens one on its floor; once the slot's streams are spent, they are retired and
/// their records removed, and the slot opens a new stream again.
#[tokio::test]
async fn test_a_slot_is_refused_past_its_share_until_its_streams_are_spent() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = runtime(&storage);
    let (flooder, honest) = (Principal::Slot(0), Principal::Slot(1));
    let class = MessageCategory::Application;

    for origin in [10, 11] {
        open(&replay, stream(origin, class), flooder, 0).await?;
    }
    assert!(pool_full(
        &open(&replay, stream(12, class), flooder, 0).await
    ));
    assert!(pool_full(
        &open(&replay, stream(12, class), Principal::Unbound, 0).await
    ));
    assert_eq!(
        open(&replay, stream(13, class), honest, 0).await?,
        SequenceVerdict::First
    );
    assert_eq!(storage.count().await?, 3);

    assert_eq!(
        open(&replay, stream(12, class), flooder, SPENT_AT_MS).await?,
        SequenceVerdict::First
    );
    // Every stream was spent: three retired, one opened.
    assert_eq!(storage.count().await?, 1);
    Ok(())
}

/// The charges survive a restart: the restored table refuses as the live one did.
#[tokio::test]
async fn test_the_charges_survive_a_restart() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let flooder = Principal::Slot(0);
    let class = MessageCategory::Storage;
    {
        let replay = runtime(&storage);
        for origin in [10, 11] {
            open(&replay, stream(origin, class), flooder, 0).await?;
        }
    }
    let restarted = runtime(&storage);
    assert!(pool_full(
        &open(&restarted, stream(12, class), flooder, 0).await
    ));
    Ok(())
}

/// Inbox mail draws on the inbox share alone: once it is full, mail from a new origin is
/// refused, while this node's own signaling still opens a stream on its local share.
#[tokio::test]
async fn test_inbox_mail_never_draws_on_the_local_share() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = runtime(&storage);
    let class = MessageCategory::Application;
    let relayed = |origin| {
        let key = stream(origin, class);
        Admission {
            introducer: Principal::Inbox,
            spent_at_ms: u128::MAX,
            ..Admission::for_test(key, 0, TransactionDigest::new([1; 32]), 0)
        }
    };
    replay.admit_relayed(relayed(10)).await?;
    assert!(matches!(
        replay.admit_relayed(relayed(11)).await,
        Err(Error::TransactionReplayStreamCapacityExceeded {
            refusal: ShareRefusal::ReservedShare { .. },
            ..
        })
    ));
    let signaling = stream(12, MessageCategory::DhtControl);
    assert_eq!(
        open(&replay, signaling, Principal::Local, 0).await?,
        SequenceVerdict::First
    );
    Ok(())
}

/// A stream held in relay custody is retired only once spent, with its custody: its entry is
/// spent no later than the stream, so both go at the stream's spent instant, and the record of
/// the stream is removed.
#[tokio::test]
async fn test_a_stream_in_custody_is_retired_only_with_its_custody() -> Result<()> {
    let storage = Arc::new(MemStorage::new());
    let replay = runtime(&storage);
    let key = stream(10, MessageCategory::Application);
    let admission = Admission {
        introducer: Principal::Inbox,
        spent_at_ms: SPENT_AT_MS,
        ..Admission::for_test(key, 0, TransactionDigest::new([1; 32]), 0)
    };
    replay.admit_relayed(admission).await?;
    let class = MessageCategory::Application;
    open(
        &replay,
        stream(11, class),
        Principal::Slot(0),
        SPENT_AT_MS - 1,
    )
    .await?;
    assert_eq!(storage.count().await?, 2);
    open(&replay, stream(12, class), Principal::Slot(1), SPENT_AT_MS).await?;
    // Both earlier streams were spent: the one in custody retired with its entry.
    assert_eq!(storage.count().await?, 1);
    Ok(())
}

/// The stream horizon (load-bounds paper, Def. 3): application mail may still be delivered out
/// of a relay inbox up to the inbox's retention after its proof expired, every other class only
/// while its proof is live, and both by a clock up to `σ` behind.
#[test]
fn test_the_spent_instant_covers_every_delivery_path() {
    use crate::consts::MAX_RELAY_INBOX_TTL_MS;
    use crate::consts::TS_OFFSET_TOLERANCE_MS;
    let (ts_ms, ttl_ms) = (5_000, 600_000);
    let expiry = ts_ms + u128::from(ttl_ms);
    for class in [
        MessageCategory::DhtControl,
        MessageCategory::Storage,
        MessageCategory::E2e,
    ] {
        assert_eq!(
            super::spent_at(class, ts_ms, ttl_ms),
            expiry + TS_OFFSET_TOLERANCE_MS
        );
    }
    let inbox = u128::from(MAX_RELAY_INBOX_TTL_MS) + TS_OFFSET_TOLERANCE_MS;
    assert_eq!(
        super::spent_at(MessageCategory::Application, ts_ms, ttl_ms),
        expiry + inbox + TS_OFFSET_TOLERANCE_MS
    );
}
