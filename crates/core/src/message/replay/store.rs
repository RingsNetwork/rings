//! Per-stream records of the replay store: the pure codec between the in-memory replay tables
//! and the key-value records that persist them.
//!
//! Every stream of either table is one record under its own storage key:
//!
//! ```text
//! record_key(table, key) ≜ "rings-core:transaction-replay:stream:" ‖ table ‖ ":" ‖ hex(enc(key))
//! record(Sender, key)    ≜ enc(Sender { key, last })
//! record(Receiver, key)  ≜ enc(Receiver { key, state })
//! ```
//!
//! **Law (locality).** A transition of one stream writes exactly that stream's record, so a
//! write is bounded by [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`] whatever the number of streams
//! retained; the whole-snapshot rewrite it replaces grew with the table.
//!
//! **Law (faithful restore).** [`restore`] ∘ `write*` is the identity on the tables: every
//! record carries its own [`StreamKey`], its storage key must be `record_key` of that key, and
//! any record that is not exactly such a pair makes the store invalid (fail closed) instead of
//! being skipped. The one tolerated foreign key is the shared-stream snapshot of the key used
//! before #898, which is reported, never decoded.
//!
//! The record is an opaque byte string to the storage, so browser storage never sees structured
//! keys or `u64` counters, and the former snapshot (also an opaque byte string) loads as a
//! record without being decoded.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use super::SequenceState;
use super::StreamKey;
use super::TRANSACTION_REPLAY_STREAM_CAPACITY;
use super::TRANSACTION_REPLAY_WINDOW;
use crate::error::Error;
use crate::error::Result;

/// Common prefix of the storage key of every stream record.
const RECORD_KEY_PREFIX: &str = "rings-core:transaction-replay:stream";
/// Storage key of the snapshot whose streams were shared by every class (before #898).
///
/// It is deleted on first load without being read: its keys cannot name a class, so it cannot
/// seed the per-class streams. Deleting it resets every replay window once, exactly as
/// deleting the replay store does.
pub(super) const SHARED_STREAM_SNAPSHOT_KEY: &str = "rings-core:transaction-replay";

/// Largest canonical encoding of a [`StreamKey`]: the network id as a `u32` varint (5), two
/// DIDs (43 each: a one-byte length and 42 hex characters) and the class tag (1).
const STREAM_KEY_MAX_BYTES: usize = 5 + 2 * 43 + 1;
/// Largest canonical encoding of a `u64` varint.
const U64_MAX_BYTES: usize = 10;
/// Largest canonical encoding of a [`SequenceState`]: `high`, then each window slot as a
/// presence tag and a 32-byte digest.
const SEQUENCE_STATE_MAX_BYTES: usize = U64_MAX_BYTES + TRANSACTION_REPLAY_WINDOW * (1 + 32);
/// Largest encoded sender stream: the variant tag, the key and the last reserved sequence.
const SENDER_STREAM_MAX_BYTES: usize = 1 + STREAM_KEY_MAX_BYTES + U64_MAX_BYTES;
/// Largest encoded receiver stream: the variant tag, the key and the window.
const RECEIVER_STREAM_MAX_BYTES: usize = 1 + STREAM_KEY_MAX_BYTES + SEQUENCE_STATE_MAX_BYTES;
/// Width of the varint length prefix of a `len`-byte string: one byte below `2^7`, two below
/// `2^14` (records and storage keys stay below `2^14`, asserted).
const fn length_prefix_bytes(len: usize) -> usize {
    if len < 1 << 7 {
        1
    } else {
        2
    }
}
/// Largest storage key of a record of `table`: the prefix, the table label and the hex key.
const fn record_key_max_bytes(table: ReplayTable) -> usize {
    RECORD_KEY_PREFIX.len() + table.label().len() + 2 + 2 * STREAM_KEY_MAX_BYTES
}
/// Largest canonical `(storage key, record)` pair of `table`, as a key-value store encodes it.
const fn stored_pair_max_bytes(table: ReplayTable, stream_bytes: usize) -> usize {
    let key_bytes = record_key_max_bytes(table);
    length_prefix_bytes(key_bytes) + key_bytes + length_prefix_bytes(stream_bytes) + stream_bytes
}

/// Largest single record of the replay store, in bytes: a receiver stream's key and full
/// window. Every replay transition writes exactly one record, so this is also the largest
/// single write of the store, whatever the number of streams it retains.
pub const TRANSACTION_REPLAY_RECORD_MAX_BYTES: usize =
    length_prefix_bytes(RECEIVER_STREAM_MAX_BYTES) + RECEIVER_STREAM_MAX_BYTES;
/// Most records a full replay store holds: one per sender and one per receiver stream.
pub const TRANSACTION_REPLAY_STORE_MAX_RECORDS: usize = 2 * TRANSACTION_REPLAY_STREAM_CAPACITY;
/// Upper bound of the canonical `(storage key, record)` encodings of a full replay store,
/// summed over its records. A byte-budgeted store must hold at least this much, or its budget
/// would evict a retained stream and reopen replay for it.
pub const TRANSACTION_REPLAY_STORE_MAX_BYTES: usize = TRANSACTION_REPLAY_STREAM_CAPACITY
    * (stored_pair_max_bytes(ReplayTable::Sender, SENDER_STREAM_MAX_BYTES)
        + stored_pair_max_bytes(ReplayTable::Receiver, RECEIVER_STREAM_MAX_BYTES));
const _: () = assert!(TRANSACTION_REPLAY_RECORD_MAX_BYTES < 1 << 14);
const _: () = assert!(record_key_max_bytes(ReplayTable::Receiver) < 1 << 14);

/// One stored record of the replay store: the opaque canonical encoding of one stream.
///
/// Only the replay runtime constructs and reads records; the storage sees a byte string.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplayRecord(pub(super) Vec<u8>);

/// The two tables of the replay store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReplayTable {
    /// Sender allocators: the last sequence reserved per stream.
    Sender,
    /// Receiver windows: the accepted-sequence window per stream.
    Receiver,
}

impl ReplayTable {
    /// The table's segment of a record's storage key.
    const fn label(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

/// The in-memory replay tables: sender allocators and receiver windows, by stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReplayTables {
    /// Last sequence reserved per sender stream.
    pub(super) sender: BTreeMap<StreamKey, u64>,
    /// Accepted-sequence window per receiver stream.
    pub(super) receiver: BTreeMap<StreamKey, SequenceState>,
}

/// One stream as a record encodes it, borrowed for encoding.
#[derive(Serialize)]
enum StoredStreamRef<'a> {
    /// A sender stream and its last reserved sequence.
    Sender { key: &'a StreamKey, last: u64 },
    /// A receiver stream and its window.
    Receiver {
        key: &'a StreamKey,
        state: &'a SequenceState,
    },
}

/// One stream as a record decodes; the owned mirror of [`StoredStreamRef`].
#[derive(Deserialize)]
enum StoredStream {
    /// A sender stream and its last reserved sequence.
    Sender { key: StreamKey, last: u64 },
    /// A receiver stream and its window, boxed so the decoded sender variant stays small; a box
    /// encodes as its contents, so the record is that of [`StoredStreamRef::Receiver`].
    Receiver {
        key: StreamKey,
        state: Box<SequenceState>,
    },
}

/// The storage key of `key`'s record in `table`.
pub(super) fn record_key(table: ReplayTable, key: &StreamKey) -> Result<String> {
    let encoded = rings_codec::serialize(key).map_err(Error::CodecSerialize)?;
    Ok(format!(
        "{RECORD_KEY_PREFIX}:{}:{}",
        table.label(),
        hex::encode(encoded)
    ))
}

/// Encode one stream as a record.
fn encode(stream: &StoredStreamRef<'_>) -> Result<ReplayRecord> {
    rings_codec::serialize(stream)
        .map(ReplayRecord)
        .map_err(Error::CodecSerialize)
}

/// The storage key and record of sender stream `key`, whose last reserved sequence is `last`.
pub(super) fn sender_record(key: &StreamKey, last: u64) -> Result<(String, ReplayRecord)> {
    Ok((
        record_key(ReplayTable::Sender, key)?,
        encode(&StoredStreamRef::Sender { key, last })?,
    ))
}

/// The storage key and record of receiver stream `key`, whose window is `state`.
pub(super) fn receiver_record(
    key: &StreamKey,
    state: &SequenceState,
) -> Result<(String, ReplayRecord)> {
    Ok((
        record_key(ReplayTable::Receiver, key)?,
        encode(&StoredStreamRef::Receiver { key, state })?,
    ))
}

/// The tables a store's records restore, and whether it still holds the former snapshot.
#[derive(Debug)]
pub(super) struct RestoredStore {
    /// The restored tables.
    pub(super) tables: ReplayTables,
    /// Whether the store holds the shared-stream snapshot of the key used before #898.
    pub(super) holds_shared_snapshot: bool,
}

/// Restore the tables from every record of a store (pure).
///
/// Fails closed with [`Error::TransactionReplayStateInvalid`] on any record that does not
/// decode, is stored under a key other than its own `record_key`, holds an invalid window, or
/// would exceed a table's stream capacity. The shared-stream snapshot is reported and never
/// decoded.
pub(super) fn restore(records: Vec<(String, ReplayRecord)>) -> Result<RestoredStore> {
    let mut restored = RestoredStore {
        tables: ReplayTables::default(),
        holds_shared_snapshot: false,
    };
    for (storage_key, ReplayRecord(bytes)) in records {
        if storage_key == SHARED_STREAM_SNAPSHOT_KEY {
            restored.holds_shared_snapshot = true;
            continue;
        }
        let stream: StoredStream =
            rings_codec::deserialize(&bytes).map_err(|_| Error::TransactionReplayStateInvalid)?;
        let (table, key) = match stream {
            StoredStream::Sender { key, last } => {
                restored.tables.sender.insert(key, last);
                (ReplayTable::Sender, key)
            }
            StoredStream::Receiver { key, state } => {
                if !state.is_valid() {
                    return Err(Error::TransactionReplayStateInvalid);
                }
                restored.tables.receiver.insert(key, *state);
                (ReplayTable::Receiver, key)
            }
        };
        if storage_key != record_key(table, &key)? {
            return Err(Error::TransactionReplayStateInvalid);
        }
    }
    if restored.tables.sender.len() > TRANSACTION_REPLAY_STREAM_CAPACITY
        || restored.tables.receiver.len() > TRANSACTION_REPLAY_STREAM_CAPACITY
    {
        return Err(Error::TransactionReplayStateInvalid);
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::receiver_record;
    use super::record_key;
    use super::record_key_max_bytes;
    use super::restore;
    use super::sender_record;
    use super::ReplayRecord;
    use super::ReplayTable;
    use super::ReplayTables;
    use super::RECEIVER_STREAM_MAX_BYTES;
    use super::SENDER_STREAM_MAX_BYTES;
    use super::SEQUENCE_STATE_MAX_BYTES;
    use super::SHARED_STREAM_SNAPSHOT_KEY;
    use super::STREAM_KEY_MAX_BYTES;
    use super::TRANSACTION_REPLAY_RECORD_MAX_BYTES;
    use super::TRANSACTION_REPLAY_STORE_MAX_BYTES;
    use super::TRANSACTION_REPLAY_STREAM_CAPACITY;
    use super::U64_MAX_BYTES;
    use crate::dht::Did;
    use crate::error::Error;
    use crate::error::Result;
    use crate::message::replay::SequenceState;
    use crate::message::replay::StreamKey;
    use crate::message::replay::TransactionDigest;
    use crate::message::replay::TRANSACTION_REPLAY_WINDOW;
    use crate::message::MessageCategory;

    /// Length of the canonical encoding of `value`.
    fn encoded_len<T: serde::Serialize>(value: &T) -> Result<usize> {
        rings_codec::serialize(value)
            .map(|bytes| bytes.len())
            .map_err(Error::CodecSerialize)
    }

    /// A stream key whose every field encodes at its maximal length.
    fn maximal_key(origin: u32) -> StreamKey {
        StreamKey::new(
            u32::MAX,
            Did::from(origin),
            Did::from(0_u32),
            MessageCategory::Application,
        )
    }

    /// A window whose every slot is occupied at the largest sequence.
    fn full_window() -> SequenceState {
        SequenceState {
            high: u64::MAX,
            accepted: [Some(TransactionDigest::new([0xff; 32])); TRANSACTION_REPLAY_WINDOW],
        }
    }

    /// The record and key bounds are exact at their maxima.
    #[test]
    fn test_record_bounds_are_exact_at_their_maxima() -> Result<()> {
        let key = maximal_key(u32::MAX);
        assert_eq!(encoded_len(&key)?, STREAM_KEY_MAX_BYTES);
        assert_eq!(encoded_len(&u64::MAX)?, U64_MAX_BYTES);
        assert_eq!(encoded_len(&full_window())?, SEQUENCE_STATE_MAX_BYTES);

        let (sender_key, sender) = sender_record(&key, u64::MAX)?;
        let (receiver_key, receiver) = receiver_record(&key, &full_window())?;
        assert_eq!(sender.0.len(), SENDER_STREAM_MAX_BYTES);
        assert_eq!(receiver.0.len(), RECEIVER_STREAM_MAX_BYTES);
        assert_eq!(encoded_len(&receiver)?, TRANSACTION_REPLAY_RECORD_MAX_BYTES);
        assert_eq!(sender_key.len(), record_key_max_bytes(ReplayTable::Sender));
        assert_eq!(
            receiver_key.len(),
            record_key_max_bytes(ReplayTable::Receiver)
        );
        // A full store of maximal pairs reaches the store bound exactly.
        let pair = encoded_len(&(sender_key, sender))? + encoded_len(&(receiver_key, receiver))?;
        assert_eq!(
            pair * TRANSACTION_REPLAY_STREAM_CAPACITY,
            TRANSACTION_REPLAY_STORE_MAX_BYTES
        );
        // The figures SECURITY.md and the replay documentation state.
        assert_eq!(TRANSACTION_REPLAY_RECORD_MAX_BYTES, 1_161);
        assert_eq!(TRANSACTION_REPLAY_STORE_MAX_BYTES, 28_295_168);
        Ok(())
    }

    /// Restore inverts the record writes, and the former snapshot is reported undecoded.
    #[test]
    fn test_restore_inverts_the_record_writes() -> Result<()> {
        let key = maximal_key(1);
        let window = full_window();
        let records = vec![
            sender_record(&key, 7)?,
            receiver_record(&key, &window)?,
            (
                SHARED_STREAM_SNAPSHOT_KEY.to_string(),
                ReplayRecord(vec![0xff; 3]),
            ),
        ];
        let restored = restore(records)?;
        let mut expected = ReplayTables::default();
        expected.sender.insert(key, 7);
        expected.receiver.insert(key, window);
        assert_eq!(restored.tables, expected);
        assert!(restored.holds_shared_snapshot);
        Ok(())
    }

    /// A record stored under another stream's key, or one that does not decode, fails closed.
    #[test]
    fn test_restore_fails_closed_on_a_misplaced_or_foreign_record() -> Result<()> {
        let (_, record) = sender_record(&maximal_key(1), 7)?;
        let misplaced = record_key(ReplayTable::Sender, &maximal_key(2))?;
        assert!(matches!(
            restore(vec![(misplaced, record.clone())]),
            Err(Error::TransactionReplayStateInvalid)
        ));
        let receiver_slot = record_key(ReplayTable::Receiver, &maximal_key(1))?;
        assert!(matches!(
            restore(vec![(receiver_slot, record)]),
            Err(Error::TransactionReplayStateInvalid)
        ));
        assert!(matches!(
            restore(vec![("foreign".to_string(), ReplayRecord(vec![0xff; 3]))]),
            Err(Error::TransactionReplayStateInvalid)
        ));
        Ok(())
    }
}
