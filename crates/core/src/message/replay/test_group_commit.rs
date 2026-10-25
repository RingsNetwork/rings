//! The group-commit law of `commit` (#916): the transitions that run while one batch is written
//! share the next batch; each is released only once its batch is durable, refusals and
//! transitions that write nothing included; a failed batch fails and undoes every pending
//! transition; leases and custody compose.
//!
//! Every wait is on an event: the first batch is held at a gate the test opens, and the test
//! probes the open batch's membership on each recorded activity (a transition joining records
//! one), never after a duration.

use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::lease::TRANSACTION_SEQUENCE_LEASE;
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
use crate::tests::activity::probe_on_activity;

/// The failure bound of every wait: a hang guard, never a pacing.
const HANG_GUARD: Duration = Duration::from_secs(10);

/// The digest of the transaction a test admits, one per `value`.
fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

/// The application stream from origin `origin` to the fixed destination 99.
fn stream(origin: u32) -> StreamKey {
    StreamKey::new(
        7,
        Did::from(origin),
        Did::from(99_u32),
        MessageCategory::Application,
    )
}

/// Hooks that record the size of every batch, hold the first batch at a gate the test opens,
/// and fail that batch iff `fail_first` is set when the gate opens.
#[derive(Default)]
struct BatchGate {
    /// The number of records of each batch written, in order.
    sizes: Mutex<Vec<usize>>,
    /// Signalled when the first batch reached the gate.
    entered: Notify,
    /// Opened by the test to let the first batch through.
    release: Notify,
    /// Whether the first batch fails once let through.
    fail_first: AtomicBool,
}

#[async_trait::async_trait]
impl StorageHooks for BatchGate {
    /// Record the batch; hold and possibly fail the first one.
    async fn before_batch(&self, keys: &[&str]) -> Result<()> {
        let first = {
            let mut sizes = self.sizes.lock().map_err(|_| Error::LockPoisoned)?;
            sizes.push(keys.len());
            sizes.len() == 1
        };
        if !first {
            return Ok(());
        }
        self.entered.notify_one();
        self.release.notified().await;
        match self.fail_first.load(Ordering::SeqCst) {
            true => Err(Error::ServiceIOError(std::io::Error::other("batch fails"))),
            false => Ok(()),
        }
    }
}

/// A memory store behind a [`BatchGate`], shared across restarts.
type Gated = Arc<Hooked<BatchGate>>;

/// A fresh gated store and a runtime over it.
fn gated() -> (Gated, Arc<TransactionReplay>) {
    let storage = Arc::new(Hooked::new(BatchGate::default()));
    let replay = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
    (storage, replay)
}

/// Admit `sequence` of `key` with the digest of `value`, detached.
fn admit(
    replay: &Arc<TransactionReplay>,
    key: StreamKey,
    sequence: u64,
    value: u8,
) -> JoinHandle<Result<SequenceVerdict>> {
    let replay = Arc::clone(replay);
    tokio::spawn(async move { replay.admit(key, sequence, digest(value)).await })
}

/// Reserve `count` sequences of `key`, detached.
fn reserve(
    replay: &Arc<TransactionReplay>,
    key: StreamKey,
    count: u64,
) -> JoinHandle<Result<RangeInclusive<u64>>> {
    let replay = Arc::clone(replay);
    tokio::spawn(async move {
        let count = NonZeroU64::new(count).ok_or(Error::TransactionReplayStateInvalid)?;
        replay.reserve(key, count).await
    })
}

/// Wait until `members` transitions wait in the open batch.
async fn open_batch_holds(replay: &Arc<TransactionReplay>, members: usize) -> Result<()> {
    probe_on_activity("the open batch's members", HANG_GUARD, || async {
        let joined = replay.state.lock().await.journal.open_members();
        Ok((joined == members).then_some(()))
    })
    .await
}

/// The outcome of a finished detached transition.
async fn outcome<T>(task: JoinHandle<Result<T>>) -> Result<T> {
    task.await
        .map_err(|_| Error::TransactionReplayStateInvalid)?
}

/// The batch sizes written so far.
fn sizes(storage: &Gated) -> Result<Vec<usize>> {
    Ok(storage
        .hooks
        .sizes
        .lock()
        .map_err(|_| Error::LockPoisoned)?
        .clone())
}

/// Batching and release: while the first batch is written, eight admissions of eight streams
/// join the next batch, which one store batch writes; none is released before the first batch
/// is durable, and every one survives a restart.
#[tokio::test]
async fn test_transitions_behind_a_write_share_one_batch() -> Result<()> {
    let (storage, replay) = gated();
    let first = admit(&replay, stream(1), 0, 1);
    storage.hooks.entered.notified().await;
    let behind = (2..10_u8)
        .map(|origin| admit(&replay, stream(u32::from(origin)), 0, origin))
        .collect::<Vec<_>>();
    open_batch_holds(&replay, behind.len()).await?;
    assert!(behind.iter().all(|task| !task.is_finished()));

    storage.hooks.release.notify_one();
    assert_eq!(outcome(first).await?, SequenceVerdict::First);
    for task in behind {
        assert_eq!(outcome(task).await?, SequenceVerdict::First);
    }
    assert_eq!(sizes(&storage)?, [1, 8]);

    let restarted = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
    for origin in 1..10_u8 {
        assert!(matches!(
            restarted
                .admit(stream(u32::from(origin)), 0, digest(origin))
                .await,
            Err(Error::TransactionReplay { .. })
        ));
    }
    Ok(())
}

/// Order and release of reads: two admissions of one stream in one batch write one record
/// holding both; a refusal computed from the batch in flight, and a reservation inside its
/// lease, which writes nothing, are released only with their batch.
#[tokio::test]
async fn test_a_batch_writes_one_record_per_stream_and_releases_every_member() -> Result<()> {
    let (storage, replay) = gated();
    let first = admit(&replay, stream(1), 0, 1);
    storage.hooks.entered.notified().await;
    let lease = reserve(&replay, stream(5), 1);
    open_batch_holds(&replay, 1).await?;
    let later = [
        admit(&replay, stream(1), 1, 2),
        admit(&replay, stream(1), 2, 3),
    ];
    let replayed = admit(&replay, stream(1), 0, 1);
    let in_lease = reserve(&replay, stream(5), 1);
    open_batch_holds(&replay, 5).await?;
    assert!(!replayed.is_finished() && !in_lease.is_finished());

    storage.hooks.release.notify_one();
    assert_eq!(outcome(first).await?, SequenceVerdict::First);
    for task in later {
        assert_eq!(outcome(task).await?, SequenceVerdict::Advance);
    }
    assert!(matches!(
        outcome(replayed).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(outcome(lease).await?, 0..=0);
    assert_eq!(outcome(in_lease).await?, 1..=1);
    // Stream 1's record once, and stream 5's lease, which covers the in-lease reservation.
    assert_eq!(sizes(&storage)?, [1, 2]);
    let restarted_lease = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
    let resumed = TRANSACTION_SEQUENCE_LEASE;
    assert_eq!(
        restarted_lease.reserve(stream(5), NonZeroU64::MIN).await?,
        resumed..=resumed
    );

    let restarted = TransactionReplay::new_shared(Box::new(Arc::clone(&storage)));
    for (sequence, value) in [(0, 1), (1, 2), (2, 3)] {
        assert!(matches!(
            restarted.admit(stream(1), sequence, digest(value)).await,
            Err(Error::TransactionReplay { .. })
        ));
    }
    Ok(())
}

/// Fail closed: a failed batch fails its own members and every transition that ran on top of
/// it, and undoes them all, so the windows, the lease and the quota are as before the batch;
/// the failure is counted once.
#[tokio::test]
async fn test_a_failed_batch_fails_and_undoes_every_pending_transition() -> Result<()> {
    let (storage, replay) = gated();
    storage.hooks.fail_first.store(true, Ordering::SeqCst);
    let first = admit(&replay, stream(1), 0, 1);
    storage.hooks.entered.notified().await;
    let on_top = admit(&replay, stream(1), 1, 2);
    let lease = reserve(&replay, stream(5), 1);
    open_batch_holds(&replay, 2).await?;

    storage.hooks.release.notify_one();
    for result in [outcome(first).await, outcome(on_top).await] {
        assert!(matches!(
            result,
            Err(Error::TransactionReplayPersistence {
                operation: "store",
                ..
            })
        ));
    }
    assert!(matches!(
        outcome(lease).await,
        Err(Error::TransactionReplayPersistence { .. })
    ));
    assert_eq!(replay.counters().persistence_failure, 1);

    assert_eq!(
        replay.admit(stream(1), 0, digest(1)).await?,
        SequenceVerdict::First
    );
    assert_eq!(replay.reserve(stream(5), NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(replay.quota_record_count_for_test().await, 1);
    Ok(())
}
