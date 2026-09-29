//! Fail-closed-per-stream replay under bad records (#909, #910): a record that does not restore
//! refuses only its own stream, the store is read once, and clearing the record, then
//! restarting, restores the stream.

use std::num::NonZeroU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::store::receiver_record;
use super::store::record_key;
use super::store::sender_record;
use super::store::ReplayTable;
use super::test_storage::Hooked;
use super::test_storage::StorageHooks;
use super::ReplayRecord;
use super::SequenceVerdict;
use super::StreamKey;
use super::TransactionDigest;
use super::TransactionReplay;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::MessageCategory;
use crate::storage::file::test_root::TempRoot;
use crate::storage::file::FileStorage;
use crate::storage::file::RecordAuthority;
use crate::storage::KvStorageInterface;
use crate::storage::KvStorageScan;

/// The digest of the transaction a test admits, one per `value`.
fn digest(value: u8) -> TransactionDigest {
    TransactionDigest::new([value; 32])
}

/// The application stream from the fixed origin `origin` to the fixed destination 99.
fn stream(origin: u32) -> StreamKey {
    StreamKey::new(
        7,
        Did::from(origin),
        Did::from(99_u32),
        MessageCategory::Application,
    )
}

/// Hooks that count the whole-store reads a load makes.
#[derive(Default)]
struct ScanCounter(AtomicUsize);

#[async_trait::async_trait]
impl StorageHooks for ScanCounter {
    /// Runs before a whole-store read.
    async fn before_scan(&self) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A memory store, shared across runtime restarts, that counts the whole-store reads.
type CountingStorage = Hooked<ScanCounter>;

impl CountingStorage {
    /// An empty counting store.
    fn counting() -> Arc<Self> {
        Arc::new(Hooked::new(ScanCounter::default()))
    }

    /// Whole-store reads (`scan` and `get_all`) made so far.
    fn scans(&self) -> usize {
        self.hooks.0.load(Ordering::SeqCst)
    }
}

/// A runtime over `storage`, as one run of a node opens it.
fn runtime(storage: &Arc<CountingStorage>) -> Arc<TransactionReplay> {
    TransactionReplay::new_shared(Box::new(Arc::clone(storage)))
}

/// A store holding a receiver record of stream 1 whose inner bytes decode as no stream, next
/// to intact sender and receiver records of stream 2.
async fn store_with_one_corrupt_record() -> Result<Arc<CountingStorage>> {
    let storage = CountingStorage::counting();
    let corrupt = record_key(ReplayTable::Receiver, &stream(1))?;
    storage
        .inner
        .put(corrupt.as_str(), &ReplayRecord(vec![0xff; 3]))
        .await?;
    let first = runtime(&storage);
    first.reserve(stream(2), NonZeroU64::MIN).await?;
    first.admit(stream(2), 0, digest(1)).await?;
    Ok(storage)
}

/// Law (fail closed per stream): the stream whose record does not restore refuses admission
/// with a typed error naming its record, counted; its sender slot and every other stream work.
#[tokio::test]
async fn test_one_corrupt_record_refuses_only_its_stream() -> Result<()> {
    let storage = store_with_one_corrupt_record().await?;
    let replay = runtime(&storage);
    let corrupt = record_key(ReplayTable::Receiver, &stream(1))?;

    assert!(matches!(
        replay.admit(stream(1), 0, digest(2)).await,
        Err(Error::TransactionReplayStreamUnavailable { key, ref record })
            if key == stream(1) && *record == corrupt
    ));
    assert_eq!(
        replay.admit(stream(2), 1, digest(3)).await?,
        SequenceVerdict::Advance
    );
    assert!(matches!(
        replay.admit(stream(2), 0, digest(1)).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(replay.reserve(stream(2), NonZeroU64::MIN).await?, 1..=1);
    assert_eq!(replay.reserve(stream(1), NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(
        replay.admit(stream(3), 0, digest(4)).await?,
        SequenceVerdict::First
    );

    let counters = replay.counters();
    assert_eq!(counters.unrestorable_record, 1);
    assert_eq!(counters.unavailable_stream, 1);
    assert_eq!(counters.persistence_failure, 0);
    // The record is kept for the operator; the refused stream never overwrote it.
    assert_eq!(
        storage.inner.get(corrupt.as_str()).await?,
        Some(ReplayRecord(vec![0xff; 3]))
    );
    Ok(())
}

/// Law (fail closed per stream), sender side: a corrupt sender record refuses reservation on
/// its stream, so no sequence of it is reused, is kept unchanged, and leaves other streams'
/// reservations and its own receiver slot working.
#[tokio::test]
async fn test_a_corrupt_sender_record_refuses_reservation_on_its_stream() -> Result<()> {
    let storage = CountingStorage::counting();
    let corrupt = record_key(ReplayTable::Sender, &stream(1))?;
    storage
        .inner
        .put(corrupt.as_str(), &ReplayRecord(vec![0xff; 3]))
        .await?;
    let replay = runtime(&storage);

    assert!(matches!(
        replay.reserve(stream(1), NonZeroU64::MIN).await,
        Err(Error::TransactionReplayStreamUnavailable { key, ref record })
            if key == stream(1) && *record == corrupt
    ));
    assert_eq!(replay.counters().unavailable_stream, 1);
    assert_eq!(
        storage.inner.get(corrupt.as_str()).await?,
        Some(ReplayRecord(vec![0xff; 3]))
    );
    assert_eq!(replay.reserve(stream(2), NonZeroU64::MIN).await?, 0..=0);
    assert_eq!(
        replay.admit(stream(1), 0, digest(1)).await?,
        SequenceVerdict::First
    );
    Ok(())
}

/// A directory occupying a native record's name cannot be read as a record: it fails its
/// stream closed without failing the load, so every other stream works and the refusal repeats
/// from the cached load, never counted as a failed read of the store.
#[tokio::test]
async fn test_an_unreadable_native_record_fails_only_its_stream() -> Result<()> {
    let root = TempRoot::new("replay-unreadable");
    let storage = FileStorage::new_with_cap_path_and_authority(
        1 << 20,
        &root,
        RecordAuthority::Authoritative,
    )
    .await?;
    let occupied = record_key(ReplayTable::Receiver, &stream(1))?;
    let name = <FileStorage as KvStorageScan<ReplayRecord>>::record_name(&storage, &occupied);
    std::fs::create_dir(root.join(&name)).map_err(Error::ServiceIOError)?;
    drop(storage);
    let replay = TransactionReplay::new_shared(Box::new(
        FileStorage::new_with_cap_path_and_authority(
            1 << 20,
            &root,
            RecordAuthority::Authoritative,
        )
        .await?,
    ));

    for sequence in 0..4_u8 {
        assert!(matches!(
            replay.admit(stream(1), u64::from(sequence), digest(sequence)).await,
            Err(Error::TransactionReplayStreamUnavailable { ref record, .. }) if *record == name
        ));
        assert!(replay
            .admit(stream(2), u64::from(sequence), digest(sequence))
            .await?
            .permits_dispatch());
    }
    let counters = replay.counters();
    assert_eq!(counters.unrestorable_record, 1);
    assert_eq!(counters.unavailable_stream, 4);
    assert_eq!(counters.persistence_failure, 0);
    drop(replay);
    Ok(())
}

/// Fail closed per stream under a misfiled native record: a copy of stream 2's receiver record
/// under stream 1's file name restores neither stream from the copy. Stream 1 is refused, named
/// by its file, and stream 2 keeps its own, newer window, so the stale copy cannot roll it back.
#[tokio::test]
async fn test_a_record_copied_over_another_stream_restores_neither() -> Result<()> {
    let root = TempRoot::new("replay-misfiled");
    let open = || {
        FileStorage::new_with_cap_path_and_authority(1 << 20, &root, RecordAuthority::Authoritative)
    };
    let (copied_from, copied_to) = {
        let storage = open().await?;
        let name =
            |key: &str| <FileStorage as KvStorageScan<ReplayRecord>>::record_name(&storage, key);
        (
            root.join(name(&record_key(ReplayTable::Receiver, &stream(2))?)),
            root.join(name(&record_key(ReplayTable::Receiver, &stream(1))?)),
        )
    };
    {
        let replay = TransactionReplay::new_shared(Box::new(open().await?));
        replay.admit(stream(2), 0, digest(1)).await?;
    }
    std::fs::copy(&copied_from, &copied_to).map_err(Error::ServiceIOError)?;
    {
        let replay = TransactionReplay::new_shared(Box::new(open().await?));
        replay.admit(stream(2), 1, digest(2)).await?;
    }

    let replay = TransactionReplay::new_shared(Box::new(open().await?));
    let named = copied_to
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    assert!(matches!(
        replay.admit(stream(1), 0, digest(1)).await,
        Err(Error::TransactionReplayStreamUnavailable { ref record, .. })
            if Some(record) == named.as_ref()
    ));
    for (sequence, value) in [(0, 1), (1, 2)] {
        assert!(matches!(
            replay.admit(stream(2), sequence, digest(value)).await,
            Err(Error::TransactionReplay { .. })
        ));
    }
    assert_eq!(replay.counters().unrestorable_record, 1);
    Ok(())
}

/// Law (restore once): the store is read by the first operation only; refused and admitted
/// calls after it read nothing, whatever their number.
#[tokio::test]
async fn test_later_calls_never_read_the_store_again() -> Result<()> {
    let storage = store_with_one_corrupt_record().await?;
    let before = storage.scans();
    let replay = runtime(&storage);

    for sequence in 0..8_u8 {
        assert!(matches!(
            replay
                .admit(stream(1), u64::from(sequence), digest(sequence))
                .await,
            Err(Error::TransactionReplayStreamUnavailable { .. })
        ));
        replay
            .admit(stream(2), u64::from(sequence) + 1, digest(sequence))
            .await?;
        replay.reserve(stream(2), NonZeroU64::MIN).await?;
    }
    assert_eq!(storage.scans() - before, 1);
    assert_eq!(replay.counters().unavailable_stream, 8);
    Ok(())
}

/// Recovery: an operator who removes the record the refusal names, then restarts the node,
/// resets that stream's replay window, and that stream alone: it admits from `First`, and the
/// other streams keep their windows.
#[tokio::test]
async fn test_clearing_the_record_restores_the_stream() -> Result<()> {
    let storage = store_with_one_corrupt_record().await?;
    let record = match runtime(&storage).admit(stream(1), 0, digest(2)).await {
        Err(Error::TransactionReplayStreamUnavailable { record, .. }) => record,
        other => panic!("the corrupt stream must be refused, got {other:?}"),
    };

    storage.inner.remove(record.as_str()).await?;
    let restarted = runtime(&storage);
    assert_eq!(
        restarted.admit(stream(1), 0, digest(2)).await?,
        SequenceVerdict::First
    );
    assert!(matches!(
        restarted.admit(stream(2), 0, digest(1)).await,
        Err(Error::TransactionReplay { .. })
    ));
    assert_eq!(restarted.counters().unrestorable_record, 0);
    Ok(())
}

/// End to end on the native store (#909 with #910): a crash leaves stream 1's receiver record
/// torn in an authoritative file store; after the restart the store reports it by its file,
/// stream 1 fails closed while stream 2 proceeds, the torn file stays, and removing that file
/// before the next restart restores stream 1.
#[tokio::test]
async fn test_a_torn_native_record_fails_its_stream_closed_until_cleared() -> Result<()> {
    let root = TempRoot::new("replay-torn");
    let open = || {
        FileStorage::new_with_cap_path_and_authority(1 << 20, &root, RecordAuthority::Authoritative)
    };
    let torn_file = {
        let storage = open().await?;
        let (window, _) = super::observe(None, 0, digest(1));
        let (torn_key, torn_record) = receiver_record(&stream(1), &window)?;
        storage.put(torn_key.as_str(), &torn_record).await?;
        let (intact_key, intact_record) = sender_record(&stream(2), 4)?;
        storage.put(intact_key.as_str(), &intact_record).await?;
        let name =
            <FileStorage as KvStorageScan<ReplayRecord>>::record_name(&storage, torn_key.as_str());
        root.join(name)
    };
    let whole = std::fs::read(&torn_file).map_err(Error::ServiceIOError)?;
    let torn = whole.get(..whole.len() / 2).unwrap_or_default();
    std::fs::write(&torn_file, torn).map_err(Error::ServiceIOError)?;

    let replay = TransactionReplay::new_shared(Box::new(open().await?));
    let named = torn_file
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    assert!(matches!(
        replay.admit(stream(1), 0, digest(1)).await,
        Err(Error::TransactionReplayStreamUnavailable { ref record, .. })
            if Some(record) == named.as_ref()
    ));
    assert_eq!(replay.reserve(stream(2), NonZeroU64::MIN).await?, 5..=5);
    assert_eq!(replay.counters().unrestorable_record, 1);
    drop(replay);
    assert!(torn_file.exists());

    std::fs::remove_file(&torn_file).map_err(Error::ServiceIOError)?;
    let restarted = TransactionReplay::new_shared(Box::new(open().await?));
    assert_eq!(
        restarted.admit(stream(1), 0, digest(1)).await?,
        SequenceVerdict::First
    );
    assert_eq!(restarted.reserve(stream(2), NonZeroU64::MIN).await?, 6..=6);
    drop(restarted);
    Ok(())
}
