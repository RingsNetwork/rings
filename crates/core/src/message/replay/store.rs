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
//! record carries its own [`StreamKey`], and a record restores its stream iff it decodes, holds a
//! valid window, and sits under `record_key` of the key it carries. The one tolerated foreign key
//! is the shared-stream snapshot of the key used before #898, which is reported, never decoded.
//!
//! **Law (fail closed per stream).** Let `name` be the storage's
//! [`record_name`](crate::storage::KvStorageScan::record_name) and `U` the names of the records
//! that do not restore: those the storage reports undecodable for their name (a torn, corrupt or
//! unreadable file, or one holding another key's record, reported by name and never deleted),
//! and those that decode but fail the conditions above. Then
//!
//! ```text
//! unavailable(table, key) ⟺ name(record_key(table, key)) ∈ U
//! ```
//!
//! and an unavailable stream refuses every transition until its record is cleared, so no replay
//! is admitted from a stream whose record is damaged, and no sequence of such a sender stream is
//! reused. The law is pointwise for the streams in the store: a record in `U` changes no other
//! restored stream's state or availability, so each keeps its replay guarantee; a new stream is
//! affected only through the bounded-slots law below. [`restore`] is therefore total: it never
//! fails as a whole.
//!
//! **Law (bounded slots).** A record in `U` may belong to either table (its name need not say),
//! so it counts against both tables' stream bounds: a table admits a new stream only while its
//! streams and `|U|` together stay below [`TRANSACTION_REPLAY_STREAM_CAPACITY`], so no new
//! stream takes the store past [`TRANSACTION_REPLAY_STORE_MAX_RECORDS`] records, however many
//! are unavailable.
//!
//! The laws cover every record the storage holds, provided the storage drops none itself: an
//! absent record, a stream's record moved or renamed away included, is indistinguishable from a
//! stream never seen (tracked in #915). The native daemon opens the replay store as an
//! authoritative file store (#909), which evicts nothing and on unix flushes each write before
//! its rename and the directory after it, so a crash leaves a record whole at its previous or its
//! new value; a record file that cannot be read, or that is not the whole record of its own key,
//! is reported by its file name and never deleted.
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
use crate::storage::ScannedRecord;
use crate::storage::UndecodableRecord;

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
/// would evict a retained stream and reopen replay for it (an evicting store) or refuse the
/// writes of new streams (an authoritative file store, which evicts nothing).
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

/// Why an entry of `U` fails its stream closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestoreFailure {
    /// The storage reports the record undecodable for its name: a torn, corrupt or unreadable
    /// record, or one holding another key's record.
    Undecodable {
        /// The key it is filed as, when its intact prefix names it.
        filed_as: Option<String>,
    },
    /// The record stored under `key` decodes as no stream.
    NotAStream {
        /// Its storage key.
        key: String,
    },
    /// The record stored under `key` holds a receiver window that violates its invariant.
    InvalidWindow {
        /// Its storage key.
        key: String,
    },
    /// The record stored under `key` decodes as a stream whose record key is another.
    Misplaced {
        /// Its storage key.
        key: String,
    },
}

/// The replay store as one load restores it: the tables and the records that fail closed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReplayStore {
    /// The restored tables.
    pub(super) tables: ReplayTables,
    /// The records that do not restore, by storage record name: the set `U` of the module
    /// documentation.
    pub(super) unrestorable: BTreeMap<String, RestoreFailure>,
}

impl ReplayStore {
    /// The record name of stream `key` of `table` if it is in `U`, so that the stream fails
    /// closed; `None` for an available stream (pure). `name` is the storage's record naming.
    /// The lookup is one record key and one map probe, and none when `U` is empty.
    pub(super) fn unavailable_record(
        &self,
        table: ReplayTable,
        key: &StreamKey,
        name: impl Fn(&str) -> String,
    ) -> Result<Option<String>> {
        if self.unrestorable.is_empty() {
            return Ok(None);
        }
        let record = name(&record_key(table, key)?);
        Ok(self.unrestorable.contains_key(&record).then_some(record))
    }

    /// Whether `table` may open one more stream: its streams and the unrestorable records,
    /// each of which may hold a slot of either table, stay below the table bound (the bounded
    /// slots law).
    pub(super) fn admits_new_stream(&self, table: ReplayTable) -> bool {
        let streams = match table {
            ReplayTable::Sender => self.tables.sender.len(),
            ReplayTable::Receiver => self.tables.receiver.len(),
        };
        streams.saturating_add(self.unrestorable.len()) < TRANSACTION_REPLAY_STREAM_CAPACITY
    }
}

/// The store a load restores, and whether it still holds the former snapshot.
#[derive(Debug)]
pub(super) struct RestoredStore {
    /// The restored tables and unrestorable records.
    pub(super) store: ReplayStore,
    /// Whether the store holds the shared-stream snapshot of the key used before #898.
    pub(super) holds_shared_snapshot: bool,
}

/// One restored stream: its table, its key and its state.
enum RestoredStream {
    /// A sender stream and its last reserved sequence.
    Sender(StreamKey, u64),
    /// A receiver stream and its window, still boxed as it decoded, so the sender variant stays
    /// small.
    Receiver(StreamKey, Box<SequenceState>),
}

/// Restore one decoded record stored under `storage_key` (pure): its stream, or why it does not
/// restore.
fn restore_record(
    storage_key: &str,
    bytes: &[u8],
) -> std::result::Result<RestoredStream, RestoreFailure> {
    let stream = match rings_codec::deserialize::<StoredStream>(bytes) {
        Ok(StoredStream::Sender { key, last }) => RestoredStream::Sender(key, last),
        Ok(StoredStream::Receiver { key, state }) if state.is_valid() => {
            RestoredStream::Receiver(key, state)
        }
        Ok(StoredStream::Receiver { .. }) => {
            return Err(RestoreFailure::InvalidWindow {
                key: storage_key.to_owned(),
            });
        }
        Err(_) => {
            return Err(RestoreFailure::NotAStream {
                key: storage_key.to_owned(),
            });
        }
    };
    let (table, key) = match &stream {
        RestoredStream::Sender(key, _) => (ReplayTable::Sender, key),
        RestoredStream::Receiver(key, _) => (ReplayTable::Receiver, key),
    };
    match record_key(table, key).is_ok_and(|own| own == storage_key) {
        true => Ok(stream),
        false => Err(RestoreFailure::Misplaced {
            key: storage_key.to_owned(),
        }),
    }
}

/// Whether `storage_key` is the key of the shared-stream snapshot used before #898.
fn is_shared_snapshot_key(storage_key: &str) -> bool {
    storage_key == SHARED_STREAM_SNAPSHOT_KEY
}

/// What one scanned record contributes to the restored store.
enum Contribution {
    /// The shared-stream snapshot of the key used before #898.
    SharedSnapshot,
    /// A restored stream.
    Stream(RestoredStream),
    /// A record that does not restore, under its storage record name.
    Unrestorable {
        /// Its storage record name.
        name: String,
        /// Why it does not restore.
        failure: RestoreFailure,
    },
}

/// Classify one scanned record (pure): the shared snapshot (named `shared_snapshot`, never
/// decoded), a restored stream, or an unrestorable record under its name.
fn classify(
    record: ScannedRecord<ReplayRecord>,
    shared_snapshot: &str,
    name: &impl Fn(&str) -> String,
) -> Contribution {
    match record {
        ScannedRecord::Undecodable(undecodable) if undecodable.name == shared_snapshot => {
            Contribution::SharedSnapshot
        }
        ScannedRecord::Undecodable(UndecodableRecord { name, key }) => Contribution::Unrestorable {
            name,
            failure: RestoreFailure::Undecodable { filed_as: key },
        },
        ScannedRecord::Filed { key, .. } if is_shared_snapshot_key(&key) => {
            Contribution::SharedSnapshot
        }
        ScannedRecord::Filed {
            key,
            value: ReplayRecord(bytes),
        } => match restore_record(&key, &bytes) {
            Ok(stream) => Contribution::Stream(stream),
            Err(failure) => Contribution::Unrestorable {
                name: name(&key),
                failure,
            },
        },
    }
}

/// Restore the store from every record a scan returned (pure and total).
///
/// `name` is the storage's record naming. Each record either restores its stream or joins the
/// unrestorable records under its name (the fail-closed-per-stream law); the shared-stream
/// snapshot is reported and never decoded, whether or not the storage could decode its framing.
///
/// ```text
/// restore = foldl step (∅, ∅, false) ∘ map classify
///
///   scanned record r ──▶ classify
///     r is the shared snapshot ──────────────▶ holds_shared_snapshot := true
///     Undecodable(r) ────────────────────────▶ U[r.name]      := Undecodable
///     Filed(k, v), restore_record = Err(f) ──▶ U[name(k)]     := f
///     Filed(k, v), restore_record = Ok(s) ───▶ tables[s]      := s.state
/// ```
pub(super) fn restore(
    records: Vec<ScannedRecord<ReplayRecord>>,
    name: impl Fn(&str) -> String,
) -> RestoredStore {
    let shared_snapshot = name(SHARED_STREAM_SNAPSHOT_KEY);
    let empty = RestoredStore {
        store: ReplayStore::default(),
        holds_shared_snapshot: false,
    };
    records
        .into_iter()
        .map(|record| classify(record, &shared_snapshot, &name))
        .fold(empty, |mut restored, contribution| {
            match contribution {
                Contribution::SharedSnapshot => restored.holds_shared_snapshot = true,
                Contribution::Stream(RestoredStream::Sender(key, last)) => {
                    restored.store.tables.sender.insert(key, last);
                }
                Contribution::Stream(RestoredStream::Receiver(key, state)) => {
                    restored.store.tables.receiver.insert(key, *state);
                }
                Contribution::Unrestorable { name, failure } => {
                    restored.store.unrestorable.insert(name, failure);
                }
            }
            restored
        })
}

#[cfg(test)]
mod tests {
    use super::receiver_record;
    use super::record_key;
    use super::record_key_max_bytes;
    use super::restore;
    use super::sender_record;
    use super::ReplayRecord;
    use super::ReplayStore;
    use super::ReplayTable;
    use super::ReplayTables;
    use super::RestoreFailure;
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
    use crate::storage::ScannedRecord;
    use crate::storage::UndecodableRecord;

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

    /// Every record scanned as filed under its own key, as a storage whose records always
    /// decode returns them.
    fn decoded(records: Vec<(String, ReplayRecord)>) -> Vec<ScannedRecord<ReplayRecord>> {
        records.into_iter().map(filed).collect()
    }

    /// One record scanned as filed under its own key.
    fn filed((key, value): (String, ReplayRecord)) -> ScannedRecord<ReplayRecord> {
        ScannedRecord::Filed { key, value }
    }

    /// One record scanned as undecodable, filed under `name`.
    fn undecodable(name: String) -> ScannedRecord<ReplayRecord> {
        ScannedRecord::Undecodable(UndecodableRecord { name, key: None })
    }

    /// The record naming of a storage that files records by their key.
    fn by_key(key: &str) -> String {
        key.to_owned()
    }

    /// The record naming of a storage that files records under a digest of their key, as the
    /// native file store does.
    fn by_digest(key: &str) -> String {
        format!("digest:{key}")
    }

    /// Restore inverts the record writes, and the former snapshot is reported undecoded.
    #[test]
    fn test_restore_inverts_the_record_writes() -> Result<()> {
        let key = maximal_key(1);
        let window = full_window();
        let records = decoded(vec![
            sender_record(&key, 7)?,
            receiver_record(&key, &window)?,
            (
                SHARED_STREAM_SNAPSHOT_KEY.to_string(),
                ReplayRecord(vec![0xff; 3]),
            ),
        ]);
        let restored = restore(records, by_key);
        let mut expected = ReplayTables::default();
        expected.sender.insert(key, 7);
        expected.receiver.insert(key, window);
        assert_eq!(restored.store.tables, expected);
        assert!(restored.store.unrestorable.is_empty());
        assert!(restored.holds_shared_snapshot);
        Ok(())
    }

    /// Fail closed per stream: a misplaced record, one decoding as no stream and one holding an
    /// invalid window each mark only their own slot unrestorable, under the name of their
    /// storage key, and restore neither the slot's stream nor the stream they carry; the other
    /// streams restore unchanged.
    #[test]
    fn test_restore_fails_closed_only_on_the_slots_of_bad_records() -> Result<()> {
        let intact = maximal_key(9);
        let (_, record) = sender_record(&maximal_key(1), 7)?;
        let misplaced = record_key(ReplayTable::Sender, &maximal_key(2))?;
        let receiver_slot = record_key(ReplayTable::Receiver, &maximal_key(1))?;
        let garbage = record_key(ReplayTable::Receiver, &maximal_key(3))?;
        let mut invalid = full_window();
        invalid.accepted = [None; TRANSACTION_REPLAY_WINDOW];
        let (invalid_slot, invalid_record) = receiver_record(&maximal_key(4), &invalid)?;
        let records = decoded(vec![
            sender_record(&intact, 3)?,
            (misplaced.clone(), record.clone()),
            (receiver_slot.clone(), record),
            (garbage.clone(), ReplayRecord(vec![0xff; 3])),
            (invalid_slot.clone(), invalid_record),
        ]);

        let restored = restore(records, by_digest);
        let expected = [
            (by_digest(&misplaced), RestoreFailure::Misplaced {
                key: misplaced.clone(),
            }),
            (by_digest(&receiver_slot), RestoreFailure::Misplaced {
                key: receiver_slot.clone(),
            }),
            (by_digest(&garbage), RestoreFailure::NotAStream {
                key: garbage.clone(),
            }),
            (by_digest(&invalid_slot), RestoreFailure::InvalidWindow {
                key: invalid_slot.clone(),
            }),
        ];
        assert_eq!(
            restored.store.unrestorable,
            expected
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        let mut tables = ReplayTables::default();
        tables.sender.insert(intact, 3);
        assert_eq!(restored.store.tables, tables);
        Ok(())
    }

    /// A record the storage cannot decode joins `U` under the storage's name, and the lookup
    /// finds exactly the stream whose record key the storage files under that name; an
    /// undecodable shared-stream snapshot is still recognised by its name and never decoded.
    #[test]
    fn test_an_undecodable_record_fails_closed_the_stream_it_is_filed_as() -> Result<()> {
        let torn = maximal_key(1);
        let torn_slot = record_key(ReplayTable::Receiver, &torn)?;
        let records = vec![
            undecodable(by_digest(&torn_slot)),
            undecodable(by_digest(SHARED_STREAM_SNAPSHOT_KEY)),
            filed(sender_record(&torn, 5)?),
        ];

        let restored = restore(records, by_digest);
        assert!(restored.holds_shared_snapshot);
        assert_eq!(
            restored.store.unrestorable.get(&by_digest(&torn_slot)),
            Some(&RestoreFailure::Undecodable { filed_as: None })
        );
        assert_eq!(
            restored
                .store
                .unavailable_record(ReplayTable::Receiver, &torn, by_digest)?,
            Some(by_digest(&torn_slot))
        );
        // The same stream's sender record restored: tables are separate slots.
        assert_eq!(
            restored
                .store
                .unavailable_record(ReplayTable::Sender, &torn, by_digest)?,
            None
        );
        assert_eq!(
            restored
                .store
                .unavailable_record(ReplayTable::Receiver, &maximal_key(2), by_digest)?,
            None
        );
        Ok(())
    }

    /// Bounded slots: each unrestorable record holds a slot of both tables, so a table admits a
    /// new stream only while its streams and `|U|` stay below the table bound.
    #[test]
    fn test_unrestorable_records_count_against_both_table_bounds() -> Result<()> {
        let mut store = ReplayStore::default();
        let capacity = u32::try_from(TRANSACTION_REPLAY_STREAM_CAPACITY)
            .map_err(|_| Error::TransactionReplayStateInvalid)?;
        for origin in 0..capacity - 1 {
            store.tables.sender.insert(maximal_key(origin), 0);
        }
        assert!(store.admits_new_stream(ReplayTable::Sender));
        assert!(store.admits_new_stream(ReplayTable::Receiver));

        store
            .unrestorable
            .insert("torn".to_owned(), RestoreFailure::Undecodable {
                filed_as: None,
            });
        assert!(!store.admits_new_stream(ReplayTable::Sender));
        assert!(store.admits_new_stream(ReplayTable::Receiver));
        Ok(())
    }
}
