//! Per-stream records of the replay store: the pure codec between the in-memory replay tables
//! and the key-value records that persist them.
//!
//! Every stream of either table is one record under its own storage key:
//!
//! ```text
//! record_key(table, key) ≜ "rings-core:transaction-replay:stream:" ‖ table ‖ ":" ‖ hex(enc(key))
//! record(Sender, key)    ≜ enc(Sender { key, leased })
//! record(Receiver, key)  ≜ enc(Receiver { key, state, custody, charge })
//! ```
//!
//! A receiver record carries its stream's charge (load-bounds paper, Law 2): the principal that
//! introduced the stream and the instant it is spent, so a restart recovers the charges.
//!
//! **Law (locality).** A transition of one stream writes exactly that stream's record, so a
//! write is bounded by [`TRANSACTION_REPLAY_RECORD_MAX_BYTES`] whatever the number of streams
//! retained; the whole-snapshot rewrite it replaces grew with the table.
//!
//! **Law (faithful restore).** [`restore`] ∘ `write*` is the identity on the receiver tables
//! and on each sender stream's lease: every record carries its own [`StreamKey`] and is written
//! under `record_key` of that key, and a record restores its stream iff it decodes and holds a
//! valid window. A sender record holds only its lease, so its allocator restores as
//! `SenderLease::restored`, the whole lease counted as issued (the laws of `lease`).
//!
//! **Law (fail closed per stream).** Let `name` be the storage's
//! [`record_name`](crate::storage::KvStorageScan::record_name) and `U` the names of the records
//! that do not restore: those the storage reports undecodable for their name (a torn, corrupt or
//! unreadable file, or one holding another key's record, reported by name and never deleted),
//! those it reports missing (witnessed as written, and no longer held), and those that decode
//! but fail the conditions above. Then
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
//! streams and `|U|` together stay below [`TRANSACTION_REPLAY_STREAM_CAPACITY`], so no new stream
//! takes the store past [`TRANSACTION_REPLAY_STORE_MAX_RECORDS`] records, however many are
//! unavailable. [`restore`] does not re-check the table bounds: admission maintains them, so only a
//! state outside the program could exceed one.
//!
//! The laws cover every record the storage witnesses, provided the storage drops none itself.
//! The native daemon opens the replay store as an authoritative file store (#909), which evicts
//! nothing, flushes each write before its rename and the directory after it (a write-through
//! rename on Windows), so a crash leaves a record whole at its previous or its new value, and
//! keeps a presence marker per record (#915), so a record removed, renamed or lost outside the
//! store is reported missing by its file name rather than read as a stream never seen; a record
//! file that cannot be read, or that is not the whole record of its own key, is reported by its
//! file name and never deleted. The browser store loses only whole databases and evicts
//! nothing (#923), so it needs no marker. A store lost whole is a stream never seen, for every
//! stream: the documented reset.
//!
//! The record is an opaque byte string to the storage, so browser storage never sees structured
//! keys or `u64` counters.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::custody::RelayCustody;
use super::custody::RELAY_CUSTODY_CAPACITY;
use super::lease::SenderLease;
use super::SequenceState;
use super::StreamKey;
use super::TRANSACTION_REPLAY_STREAM_CAPACITY;
use super::TRANSACTION_REPLAY_WINDOW;
use crate::error::Error;
use crate::error::Result;
use crate::fair_admission::Principal;
use crate::fair_admission::ShareRefusal;
use crate::fair_admission::SlotAccounting;
use crate::storage::ScannedRecord;
use crate::storage::UndecodableRecord;

/// Common prefix of the storage key of every stream record.
const RECORD_KEY_PREFIX: &str = "rings-core:transaction-replay:stream";
/// Largest canonical encoding of a [`StreamKey`]: the network id as a `u32` varint (5), two
/// DIDs (43 each: a one-byte length and 42 hex characters) and the class tag (1).
const STREAM_KEY_MAX_BYTES: usize = 5 + 2 * 43 + 1;
/// Largest canonical encoding of a `u64` varint.
const U64_MAX_BYTES: usize = 10;
/// Largest canonical encoding of a [`SequenceState`]: `high`, then each window slot as a
/// presence tag and a 32-byte digest.
const SEQUENCE_STATE_MAX_BYTES: usize = U64_MAX_BYTES + TRANSACTION_REPLAY_WINDOW * (1 + 32);
/// Largest encoded sender stream: the variant tag, the key and the leased bound.
const SENDER_STREAM_MAX_BYTES: usize = 1 + STREAM_KEY_MAX_BYTES + U64_MAX_BYTES;
/// Largest canonical encoding of one relay delivery in custody: its sequence and its digest.
const RELAY_DELIVERY_MAX_BYTES: usize = U64_MAX_BYTES + 32;
/// Largest canonical encoding of a `u128` varint.
const U128_MAX_BYTES: usize = 19;
/// Largest canonical encoding of a `u16` varint.
const U16_MAX_BYTES: usize = 3;
/// Largest canonical encoding of a [`StreamCharge`]: the principal's tag and slot, then the
/// spent instant.
const STREAM_CHARGE_MAX_BYTES: usize = 1 + U16_MAX_BYTES + U128_MAX_BYTES;
/// Largest encoded custody of one receiver stream: a one-byte length (at most
/// `RELAY_CUSTODY_CAPACITY` `< 2^7` entries, asserted) and its deliveries.
const CUSTODY_MAX_BYTES: usize = 1 + RELAY_CUSTODY_CAPACITY * RELAY_DELIVERY_MAX_BYTES;
/// Largest encoded receiver stream whose custody is empty: the variant tag, the key, the window,
/// the empty custody's length and the charge.
const RECEIVER_STREAM_UNHELD_MAX_BYTES: usize =
    1 + STREAM_KEY_MAX_BYTES + SEQUENCE_STATE_MAX_BYTES + 1 + STREAM_CHARGE_MAX_BYTES;
/// Largest encoded receiver stream: the variant tag, the key, the window, a full custody and the
/// charge.
const RECEIVER_STREAM_MAX_BYTES: usize = 1
    + STREAM_KEY_MAX_BYTES
    + SEQUENCE_STATE_MAX_BYTES
    + CUSTODY_MAX_BYTES
    + STREAM_CHARGE_MAX_BYTES;
const _: () = assert!(RELAY_CUSTODY_CAPACITY < 1 << 7);
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

/// Largest single record of the replay store, in bytes: a receiver stream's key, full window
/// and full custody. Every replay transition writes exactly one record, so this is also the
/// largest single write of the store, whatever the number of streams it retains.
pub const TRANSACTION_REPLAY_RECORD_MAX_BYTES: usize =
    length_prefix_bytes(RECEIVER_STREAM_MAX_BYTES) + RECEIVER_STREAM_MAX_BYTES;
/// Most records a full replay store holds: one per sender and one per receiver stream.
pub const TRANSACTION_REPLAY_STORE_MAX_RECORDS: usize = 2 * TRANSACTION_REPLAY_STREAM_CAPACITY;
/// Upper bound of the canonical `(storage key, record)` encodings of a full replay store,
/// summed over its records. A byte-budgeted store must hold at least this much, or its budget
/// would evict a retained stream and reopen replay for it (an evicting store) or refuse the
/// writes of new streams (an authoritative file store, which evicts nothing). The custody of
/// the whole store holds at most [`RELAY_INBOX_MAX_LEN`](crate::consts::RELAY_INBOX_MAX_LEN)
/// deliveries (the bounded custody
/// law), so they are counted once, not per stream; a receiver record's length prefix is two
/// bytes with or without them.
pub const TRANSACTION_REPLAY_STORE_MAX_BYTES: usize = TRANSACTION_REPLAY_STREAM_CAPACITY
    * (stored_pair_max_bytes(ReplayTable::Sender, SENDER_STREAM_MAX_BYTES)
        + stored_pair_max_bytes(ReplayTable::Receiver, RECEIVER_STREAM_UNHELD_MAX_BYTES))
    + RELAY_CUSTODY_CAPACITY * RELAY_DELIVERY_MAX_BYTES;
const _: () = assert!(
    length_prefix_bytes(RECEIVER_STREAM_UNHELD_MAX_BYTES)
        == length_prefix_bytes(RECEIVER_STREAM_MAX_BYTES)
);
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
    /// Sender allocators: the durable lease of sequences per stream.
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

/// The charge of one receiver stream (load-bounds paper, Law 2): the principal that introduced
/// it, and the instant it is spent, after which forgetting it reopens no replay.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct StreamCharge {
    /// The edge that delivered the stream's first transaction, or this node.
    pub(super) introducer: Principal,
    /// `x_s + ι + σ`: the instant the stream is spent (`super::spent_at`).
    pub(super) spent_at_ms: u128,
}

impl StreamCharge {
    /// Test builds: a stream of this node that is never spent.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) const fn local_for_test() -> Self {
        Self {
            introducer: Principal::Local,
            spent_at_ms: u128::MAX,
        }
    }
}

/// The in-memory replay tables: sender allocators and receiver windows, by stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ReplayTables {
    /// The allocator per sender stream: its last issued sequence and its lease.
    pub(super) sender: BTreeMap<StreamKey, SenderLease>,
    /// Accepted-sequence window per receiver stream.
    pub(super) receiver: BTreeMap<StreamKey, SequenceState>,
    /// The relay deliveries in custody per receiver stream, only for streams that hold any
    /// (the custody law); persisted in the stream's receiver record.
    pub(super) custody: BTreeMap<StreamKey, RelayCustody>,
    /// The charge of every receiver stream; persisted in its record.
    charges: BTreeMap<StreamKey, StreamCharge>,
    /// The receiver streams by the instant they are spent: `(spent_at_ms, key)` for every
    /// charge, so the spent streams are a prefix.
    spent: BTreeSet<(u128, StreamKey)>,
}

impl ReplayTables {
    /// The charge of receiver stream `key`.
    pub(super) fn charge(&self, key: &StreamKey) -> Option<StreamCharge> {
        self.charges.get(key).copied()
    }

    /// Set receiver stream `key`'s charge to `next` (`None`: none), keeping the spent index and
    /// `accounting` in step, and return the previous one: the one transition of a charge, which
    /// an undo applies with the previous charge.
    ///
    /// Law: `accounting` holds one item `(introducer, spent_at_ms)` per charge of the table.
    pub(super) fn set_charge(
        &mut self,
        key: StreamKey,
        next: Option<StreamCharge>,
        accounting: &mut SlotAccounting,
    ) -> Option<StreamCharge> {
        let previous = match next {
            Some(charge) => self.charges.insert(key, charge),
            None => self.charges.remove(&key),
        };
        if let Some(previous) = previous {
            self.spent.remove(&(previous.spent_at_ms, key));
            accounting.discharge(previous.introducer, previous.spent_at_ms);
        }
        if let Some(charge) = next {
            self.spent.insert((charge.spent_at_ms, key));
            accounting.charge(charge.introducer, charge.spent_at_ms);
        }
        previous
    }

    /// The receiver streams spent at `now_ms` that hold no relay custody (pure): forgetting one
    /// reopens no replay (Lemma 2), while a stream in custody keeps its delivery to retire.
    pub(super) fn spent_streams(&self, now_ms: u128) -> Vec<StreamKey> {
        self.spent
            .iter()
            .take_while(|(spent_at_ms, _)| *spent_at_ms <= now_ms)
            .map(|(_, key)| *key)
            .filter(|key| !self.custody.contains_key(key))
            .collect()
    }

    /// The storage key and record of receiver stream `key` as the tables hold it.
    pub(super) fn receiver_record(&self, key: &StreamKey) -> Result<(String, ReplayRecord)> {
        let state = self
            .receiver
            .get(key)
            .ok_or(Error::TransactionReplayStateInvalid)?;
        let charge = self
            .charge(key)
            .ok_or(Error::TransactionReplayStateInvalid)?;
        let custody = self.custody.get(key).cloned().unwrap_or_default();
        receiver_record(key, state, &custody, &charge)
    }

    /// Charge every receiver stream to `accounting`, as a restore found them.
    pub(super) fn charge_all(&self, accounting: &mut SlotAccounting) {
        for charge in self.charges.values() {
            accounting.charge(charge.introducer, charge.spent_at_ms);
        }
    }
}

/// One stream as a record encodes it, borrowed for encoding.
#[derive(Serialize)]
enum StoredStreamRef<'a> {
    /// A sender stream and the last sequence its lease covers.
    Sender { key: &'a StreamKey, leased: u64 },
    /// A receiver stream, its window, its relay custody and its charge.
    Receiver {
        key: &'a StreamKey,
        state: &'a SequenceState,
        custody: &'a RelayCustody,
        charge: &'a StreamCharge,
    },
}

/// One stream as a record decodes; the owned mirror of [`StoredStreamRef`].
#[derive(Deserialize)]
enum StoredStream {
    /// A sender stream and the last sequence its lease covers.
    Sender { key: StreamKey, leased: u64 },
    /// A receiver stream and its window, boxed so the decoded sender variant stays small; a box
    /// encodes as its contents, so the record is that of [`StoredStreamRef::Receiver`].
    Receiver {
        key: StreamKey,
        state: Box<SequenceState>,
        custody: RelayCustody,
        charge: StreamCharge,
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

/// The storage key and record of sender stream `key`, whose lease covers every sequence up to
/// `leased`.
pub(super) fn sender_record(key: &StreamKey, leased: u64) -> Result<(String, ReplayRecord)> {
    Ok((
        record_key(ReplayTable::Sender, key)?,
        encode(&StoredStreamRef::Sender { key, leased })?,
    ))
}

/// The storage key and record of receiver stream `key`, whose window is `state`, whose relay
/// deliveries in custody are `custody` and whose charge is `charge`.
pub(super) fn receiver_record(
    key: &StreamKey,
    state: &SequenceState,
    custody: &RelayCustody,
    charge: &StreamCharge,
) -> Result<(String, ReplayRecord)> {
    Ok((
        record_key(ReplayTable::Receiver, key)?,
        encode(&StoredStreamRef::Receiver {
            key,
            state,
            custody,
            charge,
        })?,
    ))
}

/// Why an entry of `U` fails its stream closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RestoreFailure {
    /// The storage reports the record undecodable for its name: a torn, corrupt or unreadable
    /// record, or one holding another key's record.
    Undecodable,
    /// The storage witnessed the record as written and no longer holds it: removed, renamed or
    /// lost outside the store (#915).
    Missing,
    /// The record stored under `key` decodes whole but is not a valid record of that key.
    Defective {
        /// Its storage key.
        key: String,
        /// What is wrong with it.
        defect: Defect,
    },
}

/// What is wrong with a record that decodes whole but does not restore its stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Defect {
    /// Its bytes decode as no stream.
    NotAStream,
    /// It holds a receiver window that violates its invariant.
    InvalidWindow,
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
    /// slots law); `Err(TableFull)` otherwise.
    pub(super) fn admits_new_stream(
        &self,
        table: ReplayTable,
    ) -> std::result::Result<(), ShareRefusal> {
        let streams = match table {
            ReplayTable::Sender => self.tables.sender.len(),
            ReplayTable::Receiver => self.tables.receiver.len(),
        };
        let held = streams.saturating_add(self.unrestorable.len());
        match held < TRANSACTION_REPLAY_STREAM_CAPACITY {
            true => Ok(()),
            false => Err(ShareRefusal::TableFull {
                held,
                bound: TRANSACTION_REPLAY_STREAM_CAPACITY,
            }),
        }
    }
}

/// One restored stream: its table, its key and its state.
enum RestoredStream {
    /// A sender stream and the last sequence its lease covers.
    Sender(StreamKey, u64),
    /// A receiver stream, its window (still boxed as it decoded, so the sender variant stays
    /// small), its relay custody and its charge.
    Receiver(StreamKey, Box<SequenceState>, RelayCustody, StreamCharge),
}

/// Restore one decoded record (pure): its stream, or what is wrong with it.
fn restore_record(bytes: &[u8]) -> std::result::Result<RestoredStream, Defect> {
    match rings_codec::deserialize::<StoredStream>(bytes) {
        Ok(StoredStream::Sender { key, leased }) => Ok(RestoredStream::Sender(key, leased)),
        Ok(StoredStream::Receiver {
            key,
            state,
            custody,
            charge,
        }) if state.is_valid() => Ok(RestoredStream::Receiver(key, state, custody, charge)),
        Ok(StoredStream::Receiver { .. }) => Err(Defect::InvalidWindow),
        Err(_) => Err(Defect::NotAStream),
    }
}

/// What one scanned record contributes to the restored store.
enum Contribution {
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

/// Classify one scanned record (pure): a restored stream, or an unrestorable record under its
/// name.
///
/// By the scan law a `Filed` record of key `k` is filed under `name(k)`; `name(k)` is computed
/// only for a record that does not restore.
fn classify(record: ScannedRecord<ReplayRecord>, name: &impl Fn(&str) -> String) -> Contribution {
    match record {
        ScannedRecord::Undecodable(UndecodableRecord { name }) => Contribution::Unrestorable {
            name,
            failure: RestoreFailure::Undecodable,
        },
        ScannedRecord::Missing(UndecodableRecord { name }) => Contribution::Unrestorable {
            name,
            failure: RestoreFailure::Missing,
        },
        ScannedRecord::Filed {
            key,
            value: ReplayRecord(bytes),
        } => match restore_record(&bytes) {
            Ok(stream) => Contribution::Stream(stream),
            Err(defect) => Contribution::Unrestorable {
                name: name(&key),
                failure: RestoreFailure::Defective { key, defect },
            },
        },
    }
}

/// Restore the store from every record a scan returned (pure and total).
///
/// `name` is the storage's record naming. Each record either restores its stream or joins the
/// unrestorable records under its name (the fail-closed-per-stream law).
///
/// ```text
/// restore = foldl step (∅, ∅) ∘ map classify
///
///   scanned record r ──▶ classify
///     Undecodable(r) ────────────────────────▶ U[r.name]      := Undecodable
///     Missing(r) ────────────────────────────▶ U[r.name]      := Missing
///     Filed(k, v), restore_record = Err(f) ──▶ U[name(k)]     := f
///     Filed(k, v), restore_record = Ok(s) ───▶ tables[s]      := s.state
/// ```
pub(super) fn restore(
    records: Vec<ScannedRecord<ReplayRecord>>,
    name: impl Fn(&str) -> String,
) -> ReplayStore {
    records
        .into_iter()
        .map(|record| classify(record, &name))
        .fold(ReplayStore::default(), |mut restored, contribution| {
            match contribution {
                Contribution::Stream(RestoredStream::Sender(key, leased)) => {
                    restored
                        .tables
                        .sender
                        .insert(key, SenderLease::restored(leased));
                }
                Contribution::Stream(RestoredStream::Receiver(key, state, custody, charge)) => {
                    restored.tables.receiver.insert(key, *state);
                    if !custody.is_empty() {
                        restored.tables.custody.insert(key, custody);
                    }
                    restored.tables.charges.insert(key, charge);
                    restored.tables.spent.insert((charge.spent_at_ms, key));
                }
                Contribution::Unrestorable { name, failure } => {
                    restored.unrestorable.insert(name, failure);
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
    use super::Defect;
    use super::RelayCustody;
    use super::ReplayRecord;
    use super::ReplayStore;
    use super::ReplayTable;
    use super::ReplayTables;
    use super::RestoreFailure;
    use super::SenderLease;
    use super::StreamCharge;
    use super::RECEIVER_STREAM_MAX_BYTES;
    use super::RELAY_CUSTODY_CAPACITY;
    use super::RELAY_DELIVERY_MAX_BYTES;
    use super::SENDER_STREAM_MAX_BYTES;
    use super::SEQUENCE_STATE_MAX_BYTES;
    use super::STREAM_CHARGE_MAX_BYTES;
    use super::STREAM_KEY_MAX_BYTES;
    use super::TRANSACTION_REPLAY_RECORD_MAX_BYTES;
    use super::TRANSACTION_REPLAY_STORE_MAX_BYTES;
    use super::TRANSACTION_REPLAY_STREAM_CAPACITY;
    use super::U64_MAX_BYTES;
    use crate::dht::Did;
    use crate::error::Error;
    use crate::error::Result;
    use crate::fair_admission::Principal;
    use crate::message::replay::RelayDelivery;
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

    /// A charge whose every field encodes at its maximal length.
    fn charge() -> StreamCharge {
        StreamCharge {
            introducer: Principal::Slot(u16::MAX),
            spent_at_ms: u128::MAX,
        }
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

    /// A custody of `RELAY_CUSTODY_CAPACITY` deliveries, each at its maximal encoding.
    fn full_custody() -> RelayCustody {
        (0..RELAY_CUSTODY_CAPACITY)
            .map(|slot| RelayDelivery {
                sequence: u64::MAX - u64::try_from(slot).unwrap_or_default(),
                digest: TransactionDigest::new([0xff; 32]),
            })
            .collect()
    }

    /// The record and key bounds are exact at their maxima.
    #[test]
    fn test_record_bounds_are_exact_at_their_maxima() -> Result<()> {
        let key = maximal_key(u32::MAX);
        assert_eq!(encoded_len(&key)?, STREAM_KEY_MAX_BYTES);
        assert_eq!(encoded_len(&u64::MAX)?, U64_MAX_BYTES);
        assert_eq!(encoded_len(&full_window())?, SEQUENCE_STATE_MAX_BYTES);
        assert_eq!(encoded_len(&charge())?, STREAM_CHARGE_MAX_BYTES);
        let delivery = full_custody()
            .first()
            .copied()
            .ok_or(Error::InvalidCapacity)?;
        assert_eq!(encoded_len(&delivery)?, RELAY_DELIVERY_MAX_BYTES);

        let (sender_key, sender) = sender_record(&key, u64::MAX)?;
        let (receiver_key, receiver) =
            receiver_record(&key, &full_window(), &full_custody(), &charge())?;
        assert_eq!(sender.0.len(), SENDER_STREAM_MAX_BYTES);
        assert_eq!(receiver.0.len(), RECEIVER_STREAM_MAX_BYTES);
        assert_eq!(encoded_len(&receiver)?, TRANSACTION_REPLAY_RECORD_MAX_BYTES);
        assert_eq!(sender_key.len(), record_key_max_bytes(ReplayTable::Sender));
        assert_eq!(
            receiver_key.len(),
            record_key_max_bytes(ReplayTable::Receiver)
        );
        // A full store of maximal pairs, one of which holds the whole custody of the store,
        // reaches the store bound exactly (the bounded custody law).
        let (_, unheld) = receiver_record(&key, &full_window(), &RelayCustody::new(), &charge())?;
        let unheld_pair = encoded_len(&(&receiver_key, unheld))?;
        let pair = encoded_len(&(&sender_key, sender))? + unheld_pair;
        let held = encoded_len(&(&receiver_key, receiver))? - unheld_pair;
        assert_eq!(
            pair * TRANSACTION_REPLAY_STREAM_CAPACITY + held,
            TRANSACTION_REPLAY_STORE_MAX_BYTES
        );
        // The figures the replay documentation states.
        assert_eq!(TRANSACTION_REPLAY_RECORD_MAX_BYTES, 3_873);
        assert_eq!(TRANSACTION_REPLAY_STORE_MAX_BYTES, 28_691_072);
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
        ScannedRecord::Undecodable(UndecodableRecord { name })
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

    /// Restore inverts the record writes.
    #[test]
    fn test_restore_inverts_the_record_writes() -> Result<()> {
        let key = maximal_key(1);
        let window = full_window();
        let records = decoded(vec![
            sender_record(&key, 7)?,
            receiver_record(&key, &window, &RelayCustody::new(), &charge())?,
        ]);
        let restored = restore(records, by_key);
        let mut expected = ReplayTables::default();
        expected.sender.insert(key, SenderLease::restored(7));
        expected.receiver.insert(key, window);
        expected.charges.insert(key, charge());
        expected.spent.insert((charge().spent_at_ms, key));
        assert_eq!(restored.tables, expected);
        assert!(restored.unrestorable.is_empty());
        Ok(())
    }

    /// Fail closed per stream: a record decoding as no stream and one holding an invalid window
    /// each mark only their own slot unrestorable, under the name of their storage key, and
    /// restore no stream; the other streams restore unchanged.
    #[test]
    fn test_restore_fails_closed_only_on_the_slots_of_bad_records() -> Result<()> {
        let intact = maximal_key(9);
        let garbage = record_key(ReplayTable::Receiver, &maximal_key(3))?;
        let mut invalid = full_window();
        invalid.accepted = [None; TRANSACTION_REPLAY_WINDOW];
        let (invalid_slot, invalid_record) =
            receiver_record(&maximal_key(4), &invalid, &RelayCustody::new(), &charge())?;
        let records = decoded(vec![
            sender_record(&intact, 3)?,
            (garbage.clone(), ReplayRecord(vec![0xff; 3])),
            (invalid_slot.clone(), invalid_record),
        ]);

        let restored = restore(records, by_digest);
        let expected = [
            (by_digest(&garbage), RestoreFailure::Defective {
                key: garbage.clone(),
                defect: Defect::NotAStream,
            }),
            (by_digest(&invalid_slot), RestoreFailure::Defective {
                key: invalid_slot.clone(),
                defect: Defect::InvalidWindow,
            }),
        ];
        assert_eq!(
            restored.unrestorable,
            expected
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        );
        let mut tables = ReplayTables::default();
        tables.sender.insert(intact, SenderLease::restored(3));
        assert_eq!(restored.tables, tables);
        Ok(())
    }

    /// A record the storage cannot decode joins `U` under the storage's name, and the lookup
    /// finds exactly the stream whose record key the storage files under that name.
    #[test]
    fn test_an_undecodable_record_fails_closed_the_stream_it_is_filed_as() -> Result<()> {
        let torn = maximal_key(1);
        let torn_slot = record_key(ReplayTable::Receiver, &torn)?;
        let records = vec![
            undecodable(by_digest(&torn_slot)),
            filed(sender_record(&torn, 5)?),
        ];

        let restored = restore(records, by_digest);
        assert_eq!(
            restored.unrestorable.get(&by_digest(&torn_slot)),
            Some(&RestoreFailure::Undecodable)
        );
        assert_eq!(
            restored.unavailable_record(ReplayTable::Receiver, &torn, by_digest)?,
            Some(by_digest(&torn_slot))
        );
        // The same stream's sender record restored: tables are separate slots.
        assert_eq!(
            restored.unavailable_record(ReplayTable::Sender, &torn, by_digest)?,
            None
        );
        assert_eq!(
            restored.unavailable_record(ReplayTable::Receiver, &maximal_key(2), by_digest)?,
            None
        );
        Ok(())
    }

    /// The state one stream's record is in, for the exhaustive pointwise check.
    #[derive(Clone, Copy, Debug)]
    enum RecordState {
        /// A valid record of the stream, under its own key.
        Intact,
        /// Bytes that decode as no stream.
        NotAStream,
        /// A receiver window that violates its invariant.
        InvalidWindow,
        /// A record the storage cannot decode.
        Undecodable,
    }

    /// Every state a record can be in.
    const RECORD_STATES: [RecordState; 4] = [
        RecordState::Intact,
        RecordState::NotAStream,
        RecordState::InvalidWindow,
        RecordState::Undecodable,
    ];

    /// The record of `stream` in a table: a sender record (its last sequence) or a receiver
    /// record (a full window), as `(storage key, record)`.
    fn table_record(table: ReplayTable, stream: &StreamKey) -> Result<(String, ReplayRecord)> {
        match table {
            ReplayTable::Sender => sender_record(stream, 7),
            ReplayTable::Receiver => {
                receiver_record(stream, &full_window(), &RelayCustody::new(), &charge())
            }
        }
    }

    /// The scanned record of `stream` in `table` and `state`, and the entry of `U` it must
    /// produce (none for an intact record).
    fn record_in(
        table: ReplayTable,
        stream: &StreamKey,
        state: RecordState,
    ) -> Result<(ScannedRecord<ReplayRecord>, Option<RestoreFailure>)> {
        let own = record_key(table, stream)?;
        let defective = |defect| RestoreFailure::Defective {
            key: own.clone(),
            defect,
        };
        Ok(match state {
            RecordState::Intact => (filed(table_record(table, stream)?), None),
            RecordState::NotAStream => (
                filed((own.clone(), ReplayRecord(vec![0xff; 3]))),
                Some(defective(Defect::NotAStream)),
            ),
            RecordState::InvalidWindow => {
                let mut invalid = full_window();
                invalid.accepted = [None; TRANSACTION_REPLAY_WINDOW];
                let (_, record) =
                    receiver_record(stream, &invalid, &RelayCustody::new(), &charge())?;
                (
                    filed((own.clone(), record)),
                    Some(defective(Defect::InvalidWindow)),
                )
            }
            RecordState::Undecodable => (
                undecodable(by_digest(&own)),
                Some(RestoreFailure::Undecodable),
            ),
        })
    }

    /// The fail-closed-per-stream law, checked exhaustively over three streams, each holding one
    /// record in either table, in every state (8 choices each, 512 stores): the tables are
    /// exactly those of the intact records alone, and `U` holds exactly the bad records, each
    /// under its own name with its own failure, whatever the other streams' records.
    #[test]
    fn test_restore_is_pointwise_over_every_store_of_three_streams() -> Result<()> {
        let keys = [maximal_key(1), maximal_key(2), maximal_key(3)];
        let choices = [ReplayTable::Sender, ReplayTable::Receiver]
            .into_iter()
            .flat_map(|table| RECORD_STATES.into_iter().map(move |state| (table, state)))
            .collect::<Vec<_>>();
        for first in choices.iter() {
            for second in choices.iter() {
                for third in choices.iter() {
                    let store = [*first, *second, *third];
                    let mut records = Vec::new();
                    let mut intact = Vec::new();
                    let mut expected = std::collections::BTreeMap::new();
                    for (index, (table, state)) in store.into_iter().enumerate() {
                        let stream = &keys[index];
                        let (record, failure) = record_in(table, stream, state)?;
                        match failure {
                            None => intact.push(record.clone()),
                            Some(failure) => {
                                expected.insert(by_digest(&record_key(table, stream)?), failure);
                            }
                        }
                        records.push(record);
                    }
                    let restored = restore(records, by_digest);
                    assert_eq!(
                        restored.tables,
                        restore(intact, by_digest).tables,
                        "{store:?}"
                    );
                    assert_eq!(restored.unrestorable, expected, "{store:?}");
                }
            }
        }
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
            store
                .tables
                .sender
                .insert(maximal_key(origin), SenderLease::restored(0));
        }
        assert!(store.admits_new_stream(ReplayTable::Sender).is_ok());
        assert!(store.admits_new_stream(ReplayTable::Receiver).is_ok());

        store
            .unrestorable
            .insert("torn".to_owned(), RestoreFailure::Undecodable);
        assert!(store.admits_new_stream(ReplayTable::Sender).is_err());
        assert!(store.admits_new_stream(ReplayTable::Receiver).is_ok());
        Ok(())
    }
}
