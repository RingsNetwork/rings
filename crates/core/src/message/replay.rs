//! Destination-scoped replay protection for signed transactions.
//!
//! The stream identity is `(network_id, origin account DID, destination DID)`. A receiver keeps
//! a fixed-width window per stream, so delivery may be reordered within the window without
//! turning a normal gap into an omission claim. The state transition is pure in [`observe`];
//! [`TransactionReplay`] is the effect boundary that serializes transitions and persists an
//! admitted state before returning it to the inbound dispatcher.
//!
//! Persistence is one versioned snapshot under one storage key. Sender and receiver tables each
//! have a hard stream-count bound and never evict: once the bound is reached, a new stream fails
//! closed. Existing stream records remain durable until an operator explicitly removes the
//! replay store. Deleting that store deletes the corresponding replay guarantee. Runtime-local
//! origin quota state shares the serialized receiver commit boundary but is not part of the
//! snapshot.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures::lock::Mutex;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::quota::quota_admission_error;
use crate::message::quota::OriginQuotaCounterState;
use crate::message::quota::OriginQuotaTable;
use crate::message::types::MessageCategory;
use crate::message::OriginQuotaCharge;
use crate::message::OriginQuotaConfig;
use crate::message::OriginQuotaCounters;
use crate::message::OriginQuotaInstant;
use crate::message::OriginQuotaKey;
use crate::storage::KvStorageInterface;
use crate::utils::Instant;

/// Number of out-of-order sequence slots retained for one destination-scoped stream.
pub const TRANSACTION_REPLAY_WINDOW: usize = 32;
const TRANSACTION_REPLAY_WINDOW_U64: u64 = 32;
const TRANSACTION_REPLAY_BACKTRACK: u64 = 31;
/// Maximum sender streams and maximum receiver streams retained by one runtime.
pub const TRANSACTION_REPLAY_STREAM_CAPACITY: usize = 4096;
/// Storage key of the replay snapshot: one sequence stream per `(origin, destination, class)`.
const TRANSACTION_REPLAY_SNAPSHOT_KEY: &str = "rings-core:transaction-replay:class-streams";
/// Storage key of the snapshot whose streams were shared by every class (before #898).
///
/// It is deleted on first load without being read: its keys cannot name a class, so it cannot
/// seed the per-class streams. Deleting it resets every replay window once, exactly as
/// deleting the replay store does.
const SHARED_STREAM_SNAPSHOT_KEY: &str = "rings-core:transaction-replay";

/// A destination-scoped transaction stream of one traffic class (#898).
///
/// Sequences are ordered only per class. The outbound scheduler preserves order inside a
/// class lane and deliberately reorders across lanes, so a stream shared by every class let a
/// backlog in one lane fall behind later-sequenced traffic of another lane and be rejected as
/// stale. With one stream per class, an honest sender's transactions reach the receiver in
/// sequence order up to the lane's in-flight window, which is below the replay window.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct StreamKey {
    /// Overlay in which the transaction signature is valid.
    pub network_id: u32,
    /// Account DID recovered from the transaction's delegation.
    pub origin_account: Did,
    /// Final logical destination of the transaction.
    pub destination: Did,
    /// Traffic class implied by the transaction's signed data.
    pub class: MessageCategory,
}

impl StreamKey {
    /// Name one account-to-destination stream of `class` inside an overlay.
    pub const fn new(
        network_id: u32,
        origin_account: Did,
        destination: Did,
        class: MessageCategory,
    ) -> Self {
        Self {
            network_id,
            origin_account,
            destination,
            class,
        }
    }
}

/// Digest of one exact signed transaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TransactionDigest(pub(crate) [u8; 32]);

impl TransactionDigest {
    /// Construct a digest from its canonical 32-byte representation.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the canonical digest bytes.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// The two exact signed-transaction digests retained for a sequence fork.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionForkEvidence {
    /// Digest already retained for the sequence.
    pub accepted: TransactionDigest,
    /// Digest presented by the conflicting transaction.
    pub incoming: TransactionDigest,
}

/// Fixed-width accepted-sequence window for one stream.
///
/// `accepted.last()` is sequence `high`; preceding slots descend toward
/// `high.saturating_sub(31)`. Slots below sequence zero are necessarily empty.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SequenceState {
    high: u64,
    accepted: [Option<TransactionDigest>; TRANSACTION_REPLAY_WINDOW],
}

impl SequenceState {
    fn first(sequence: u64, digest: TransactionDigest) -> Self {
        let mut accepted = [None; TRANSACTION_REPLAY_WINDOW];
        if let Some(slot) = accepted.last_mut() {
            *slot = Some(digest);
        }
        Self {
            high: sequence,
            accepted,
        }
    }

    /// Highest sequence observed for this stream.
    pub const fn high(&self) -> u64 {
        self.high
    }

    /// Lowest sequence whose digest may still be retained.
    pub const fn retained_min(&self) -> u64 {
        self.high.saturating_sub(TRANSACTION_REPLAY_BACKTRACK)
    }

    fn is_valid(&self) -> bool {
        let retained_slots = self
            .high
            .checked_add(1)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(TRANSACTION_REPLAY_WINDOW)
            .min(TRANSACTION_REPLAY_WINDOW);
        let unused_slots = TRANSACTION_REPLAY_WINDOW.saturating_sub(retained_slots);
        self.accepted.iter().take(unused_slots).all(Option::is_none)
            && self.accepted.last().is_some_and(Option::is_some)
    }

    fn advance(&mut self, sequence: u64, digest: TransactionDigest) {
        let gap = sequence.saturating_sub(self.high);
        if gap >= TRANSACTION_REPLAY_WINDOW_U64 {
            self.accepted.fill(None);
        } else if let Ok(gap) = usize::try_from(gap) {
            self.accepted.rotate_left(gap);
            let retained = TRANSACTION_REPLAY_WINDOW.saturating_sub(gap);
            for slot in self.accepted.iter_mut().skip(retained) {
                *slot = None;
            }
        }
        self.high = sequence;
        if let Some(slot) = self.accepted.last_mut() {
            *slot = Some(digest);
        }
    }

    fn observe_retained(&mut self, sequence: u64, digest: TransactionDigest) -> SequenceVerdict {
        let distance = self.high.saturating_sub(sequence);
        let Ok(distance) = usize::try_from(distance) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        let Some(index) = TRANSACTION_REPLAY_WINDOW.checked_sub(distance.saturating_add(1)) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        let Some(slot) = self.accepted.get_mut(index) else {
            return SequenceVerdict::Stale {
                retained_min: self.retained_min(),
            };
        };
        match *slot {
            None => {
                *slot = Some(digest);
                SequenceVerdict::Late
            }
            Some(accepted) if accepted == digest => SequenceVerdict::Replay,
            Some(accepted) => SequenceVerdict::Fork {
                accepted,
                incoming: digest,
            },
        }
    }
}

/// Typed result of observing one sequence and exact signed-transaction digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SequenceVerdict {
    /// No retained state existed; this transaction established the stream baseline.
    First,
    /// The sequence advanced the high watermark, possibly across a normal delivery gap.
    Advance,
    /// The sequence filled an empty slot inside the retained reordering window.
    Late,
    /// The same exact signed transaction was already retained at this sequence.
    Replay,
    /// A different signed transaction was already retained at this sequence.
    Fork {
        /// Digest retained for the sequence.
        accepted: TransactionDigest,
        /// Digest presented by the new transaction.
        incoming: TransactionDigest,
    },
    /// The sequence fell below the retained window.
    Stale {
        /// Lowest sequence that may still be retained.
        retained_min: u64,
    },
}

impl SequenceVerdict {
    /// Whether this observation may cross the application dispatch boundary.
    pub const fn permits_dispatch(self) -> bool {
        matches!(self, Self::First | Self::Advance | Self::Late)
    }
}

/// Apply the pure replay-window transition.
///
/// Transition shape: `Option<SequenceState> -> (sequence, digest) -> (SequenceState, verdict)`.
/// Rejected observations return the original state unchanged.
pub fn observe(
    state: Option<SequenceState>,
    sequence: u64,
    digest: TransactionDigest,
) -> (SequenceState, SequenceVerdict) {
    let Some(mut state) = state else {
        return (
            SequenceState::first(sequence, digest),
            SequenceVerdict::First,
        );
    };
    if sequence > state.high {
        state.advance(sequence, digest);
        return (state, SequenceVerdict::Advance);
    }
    if sequence < state.retained_min() {
        let retained_min = state.retained_min();
        return (state, SequenceVerdict::Stale { retained_min });
    }
    let verdict = state.observe_retained(sequence, digest);
    (state, verdict)
}

/// Versioned durable sender and receiver state.
///
/// Fields are private so only the transaction replay runtime can apply the capacity and
/// persistence laws. The Serde representation is an opaque Rings-codec byte sequence: browser
/// storage therefore never exposes structured map keys or `u64` counters to JavaScript's JSON
/// number and object-key restrictions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplaySnapshot {
    sender: BTreeMap<StreamKey, u64>,
    receiver: BTreeMap<StreamKey, SequenceState>,
}

#[derive(Serialize)]
struct ReplaySnapshotRef<'a> {
    sender: &'a BTreeMap<StreamKey, u64>,
    receiver: &'a BTreeMap<StreamKey, SequenceState>,
}

#[derive(Deserialize)]
struct ReplaySnapshotWire {
    sender: BTreeMap<StreamKey, u64>,
    receiver: BTreeMap<StreamKey, SequenceState>,
}

impl Serialize for ReplaySnapshot {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where S: serde::Serializer {
        let wire = ReplaySnapshotRef {
            sender: &self.sender,
            receiver: &self.receiver,
        };
        let encoded = rings_codec::serialize(&wire).map_err(serde::ser::Error::custom)?;
        encoded.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ReplaySnapshot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        let encoded = Vec::<u8>::deserialize(deserializer)?;
        let wire: ReplaySnapshotWire =
            rings_codec::deserialize(&encoded).map_err(serde::de::Error::custom)?;
        Ok(Self {
            sender: wire.sender,
            receiver: wire.receiver,
        })
    }
}

/// Storage accepted by the transaction replay runtime.
pub type ReplayStorage =
    Box<rings_runtime::maybe_send_sync!(dyn KvStorageInterface<ReplaySnapshot>)>;

/// Observable rejected-verdict and persistence-failure counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayCounters {
    /// Exact duplicate transactions rejected.
    pub replay: u64,
    /// Conflicting transactions at one sequence rejected.
    pub fork: u64,
    /// Transactions below the retained window rejected.
    pub stale: u64,
    /// Replay snapshot reads or writes that failed.
    pub persistence_failure: u64,
}

#[derive(Default)]
struct ReplayCounterState {
    replay: AtomicU64,
    fork: AtomicU64,
    stale: AtomicU64,
    persistence_failure: AtomicU64,
}

impl ReplayCounterState {
    fn snapshot(&self) -> ReplayCounters {
        ReplayCounters {
            replay: self.replay.load(Ordering::Relaxed),
            fork: self.fork.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            persistence_failure: self.persistence_failure.load(Ordering::Relaxed),
        }
    }
}

/// Serialized sender allocator and receiver replay-window effect boundary.
pub(crate) struct TransactionReplay {
    storage: ReplayStorage,
    state: Mutex<TransactionAdmissionState>,
    started_at: Instant,
    counters: ReplayCounterState,
    quota_counters: OriginQuotaCounterState,
}

struct TransactionAdmissionState {
    snapshot: Option<ReplaySnapshot>,
    quota: OriginQuotaTable,
}

impl TransactionReplay {
    /// Construct a replay runtime. The snapshot is loaded lazily on its first operation.
    #[cfg(test)]
    pub(crate) fn new(storage: ReplayStorage) -> Self {
        Self::new_with_quota(storage, OriginQuotaConfig::default())
    }

    /// Construct a replay runtime with explicit runtime-local origin quotas.
    pub(crate) fn new_with_quota(storage: ReplayStorage, quota_config: OriginQuotaConfig) -> Self {
        Self {
            storage,
            state: Mutex::new(TransactionAdmissionState {
                snapshot: None,
                quota: OriginQuotaTable::new(quota_config),
            }),
            started_at: Instant::now(),
            counters: ReplayCounterState::default(),
            quota_counters: OriginQuotaCounterState::default(),
        }
    }

    /// Current observable counters.
    pub(crate) fn counters(&self) -> ReplayCounters {
        self.counters.snapshot()
    }

    /// Current aggregate origin-quota rejection counters.
    pub(crate) fn quota_counters(&self) -> OriginQuotaCounters {
        self.quota_counters.snapshot()
    }

    async fn load_snapshot<'a>(
        &self,
        slot: &'a mut Option<ReplaySnapshot>,
    ) -> Result<&'a mut ReplaySnapshot> {
        if slot.is_none() {
            // Removing an absent key succeeds, so every first load repeats this idempotently.
            if let Err(source) = self.storage.remove(SHARED_STREAM_SNAPSHOT_KEY).await {
                self.counters
                    .persistence_failure
                    .fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionReplayPersistence {
                    operation: "remove shared-stream snapshot",
                    source: Box::new(source),
                });
            }
            let loaded = match self.storage.get(TRANSACTION_REPLAY_SNAPSHOT_KEY).await {
                Ok(snapshot) => snapshot.unwrap_or_default(),
                Err(source) => {
                    self.counters
                        .persistence_failure
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(Error::TransactionReplayPersistence {
                        operation: "load",
                        source: Box::new(source),
                    });
                }
            };
            if loaded.sender.len() > TRANSACTION_REPLAY_STREAM_CAPACITY
                || loaded.receiver.len() > TRANSACTION_REPLAY_STREAM_CAPACITY
                || loaded.receiver.values().any(|state| !state.is_valid())
            {
                self.counters
                    .persistence_failure
                    .fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionReplayStateInvalid);
            }
            *slot = Some(loaded);
        }
        slot.as_mut().ok_or(Error::TransactionReplayStateInvalid)
    }

    async fn persist(&self, snapshot: &ReplaySnapshot) -> Result<()> {
        self.storage
            .put(TRANSACTION_REPLAY_SNAPSHOT_KEY, snapshot)
            .await
            .map_err(|source| {
                self.counters
                    .persistence_failure
                    .fetch_add(1, Ordering::Relaxed);
                Error::TransactionReplayPersistence {
                    operation: "store",
                    source: Box::new(source),
                }
            })
    }

    /// Reserve and persist `count` sender sequences before returning the range to the signer.
    pub(crate) async fn reserve(
        &self,
        key: StreamKey,
        count: NonZeroU64,
    ) -> Result<RangeInclusive<u64>> {
        let mut state = self.state.lock().await;
        let snapshot = self.load_snapshot(&mut state.snapshot).await?;
        let previous = snapshot.sender.get(&key).copied();
        if previous.is_none() && snapshot.sender.len() >= TRANSACTION_REPLAY_STREAM_CAPACITY {
            return Err(Error::TransactionReplayStreamCapacityExceeded {
                capacity: TRANSACTION_REPLAY_STREAM_CAPACITY,
            });
        }
        let first = match previous {
            Some(last) => last
                .checked_add(1)
                .ok_or(Error::TransactionSequenceExhausted { key })?,
            None => 0,
        };
        let last = first
            .checked_add(count.get().saturating_sub(1))
            .ok_or(Error::TransactionSequenceExhausted { key })?;
        snapshot.sender.insert(key, last);
        if let Err(error) = self.persist(snapshot).await {
            match previous {
                Some(previous) => {
                    snapshot.sender.insert(key, previous);
                }
                None => {
                    snapshot.sender.remove(&key);
                }
            }
            return Err(error);
        }
        Ok(first..=last)
    }

    /// Atomically commit replay classification and origin-quota admission before dispatch.
    pub(crate) async fn admit_with_quota(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        charge: OriginQuotaCharge,
        byte_cost: usize,
    ) -> Result<SequenceVerdict> {
        let now = OriginQuotaInstant::from_nanos(
            Instant::now()
                .saturating_duration_since(self.started_at)
                .as_nanos(),
        );
        self.admit_at(key, sequence, digest, charge, byte_cost, now)
            .await
    }

    async fn admit_at(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
        charge: OriginQuotaCharge,
        byte_cost: usize,
        now: OriginQuotaInstant,
    ) -> Result<SequenceVerdict> {
        let mut state = self.state.lock().await;
        self.load_snapshot(&mut state.snapshot).await?;
        let previous = state
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.receiver.get(&key).cloned());
        let receiver_len = state
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.receiver.len());
        if previous.is_none() && receiver_len >= TRANSACTION_REPLAY_STREAM_CAPACITY {
            return Err(Error::TransactionReplayStreamCapacityExceeded {
                capacity: TRANSACTION_REPLAY_STREAM_CAPACITY,
            });
        }
        let (next, verdict) = observe(previous.clone(), sequence, digest);
        match verdict {
            SequenceVerdict::First | SequenceVerdict::Advance | SequenceVerdict::Late => {}
            SequenceVerdict::Replay => {
                self.counters.replay.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionReplay { key, sequence });
            }
            SequenceVerdict::Fork { accepted, incoming } => {
                self.counters.fork.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionSequenceFork {
                    key,
                    sequence,
                    evidence: Box::new(TransactionForkEvidence { accepted, incoming }),
                });
            }
            SequenceVerdict::Stale { retained_min } => {
                self.counters.stale.fetch_add(1, Ordering::Relaxed);
                return Err(Error::TransactionSequenceStale {
                    key,
                    sequence,
                    retained_min,
                });
            }
        }

        let quota_key = OriginQuotaKey::new(
            key.network_id,
            key.origin_account,
            key.destination,
            charge.lane,
        );
        let reservation = state
            .quota
            .reserve(quota_key, charge.message_limit, byte_cost, now);
        let quota_reservation = match reservation {
            Ok(reservation) => reservation,
            Err(error) => {
                self.quota_counters.record(charge.lane, &error);
                return Err(quota_admission_error(quota_key, byte_cost, error));
            }
        };
        let Some(snapshot) = state.snapshot.as_mut() else {
            quota_reservation.rollback(&mut state.quota);
            return Err(Error::TransactionReplayStateInvalid);
        };
        snapshot.receiver.insert(key, next);
        if let Err(error) = self.persist(snapshot).await {
            match previous {
                Some(previous) => {
                    snapshot.receiver.insert(key, previous);
                }
                None => {
                    snapshot.receiver.remove(&key);
                }
            }
            quota_reservation.rollback(&mut state.quota);
            return Err(error);
        }
        Ok(verdict)
    }

    #[cfg(test)]
    async fn admit(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
    ) -> Result<SequenceVerdict> {
        self.admit_at(
            key,
            sequence,
            digest,
            crate::message::MessageCategory::Application.into(),
            0,
            OriginQuotaInstant::ZERO,
        )
        .await
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) async fn quota_record_count_for_test(&self) -> usize {
        self.state.lock().await.quota.len()
    }

    /// The whole `(message, byte)` tokens `key`'s quota record held after its last admission.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) async fn quota_tokens_for_test(&self, key: OriginQuotaKey) -> Option<(u128, u128)> {
        self.state.lock().await.quota.whole_tokens(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn digest(value: u8) -> TransactionDigest {
        TransactionDigest::new([value; 32])
    }

    #[cfg(not(target_family = "wasm"))]
    fn stream(destination: Did) -> StreamKey {
        StreamKey::new(
            7,
            SecretKey::random().address().into(),
            destination,
            MessageCategory::Application,
        )
    }

    #[test]
    fn duplicate_late_stale_fork_and_gap_transitions_are_typed() {
        let (state, first) = observe(None, 10, digest(1));
        assert_eq!(first, SequenceVerdict::First);

        let (state, advanced) = observe(Some(state), 12, digest(2));
        assert_eq!(advanced, SequenceVerdict::Advance);

        let (state, late) = observe(Some(state), 11, digest(3));
        assert_eq!(late, SequenceVerdict::Late);

        let (state, replay) = observe(Some(state), 11, digest(3));
        assert_eq!(replay, SequenceVerdict::Replay);

        let (state, fork) = observe(Some(state), 11, digest(4));
        assert_eq!(fork, SequenceVerdict::Fork {
            accepted: digest(3),
            incoming: digest(4),
        });

        let (state, gap) = observe(Some(state), 100, digest(5));
        assert_eq!(gap, SequenceVerdict::Advance);
        let (_, stale) = observe(Some(state), 68, digest(6));
        assert_eq!(stale, SequenceVerdict::Stale { retained_min: 69 });
    }

    /// One transaction as the receiver sees it: its class and its per-class and shared
    /// sequence numbers.
    #[derive(Clone, Copy)]
    struct Sent {
        /// Traffic class of the transaction.
        class: MessageCategory,
        /// Its sequence in its class's stream (#898).
        class_sequence: u64,
        /// Its sequence in a stream shared by every class (before #898).
        shared_sequence: u64,
    }

    /// A deterministic 64-bit xorshift step.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// One arrival order of an honest sender's traffic, from a fixed `seed`.
    ///
    /// The sender signs each class's transactions in sequence order, spread over four classes
    /// and one shared counter. The scheduler then serves the class lanes in an arbitrary
    /// interleaving: each lane stays FIFO up to its in-flight window, whose frames may still
    /// arrive in any order (the lane window is below the replay window), and the lanes are
    /// merged in any order, which lets one lane fall far behind the others.
    fn arrivals(seed: u64, per_class: u64, lane_window: usize) -> Vec<Sent> {
        let classes = [
            MessageCategory::DhtControl,
            MessageCategory::Storage,
            MessageCategory::E2e,
            MessageCategory::Application,
        ];
        let mut state = seed | 1;
        let mut lanes: Vec<Vec<Sent>> = vec![Vec::new(); classes.len()];
        let mut class_next = [0_u64; 4];
        for shared_sequence in 0..per_class * classes.len() as u64 {
            let lane = usize::try_from(xorshift(&mut state) % 4).expect("small index");
            let class_sequence = class_next[lane];
            class_next[lane] += 1;
            lanes[lane].push(Sent {
                class: classes[lane],
                class_sequence,
                shared_sequence,
            });
        }
        for lane in lanes.iter_mut() {
            for window in lane.chunks_mut(lane_window) {
                for index in (1..window.len()).rev() {
                    let other =
                        usize::try_from(xorshift(&mut state) % (index as u64 + 1)).expect("index");
                    window.swap(index, other);
                }
            }
        }
        let mut cursors = [0_usize; 4];
        let mut merged = Vec::new();
        while cursors
            .iter()
            .zip(lanes.iter())
            .any(|(cursor, lane)| *cursor < lane.len())
        {
            // Favour one lane in long bursts so the others build a backlog behind it.
            let lane = usize::try_from(xorshift(&mut state) % 4).expect("small index");
            let burst = xorshift(&mut state) % 64;
            for _ in 0..burst {
                if let Some(sent) = lanes[lane].get(cursors[lane]) {
                    merged.push(*sent);
                    cursors[lane] += 1;
                }
            }
        }
        merged
    }

    /// Law of #898: with one stream per `(origin, destination, class)`, an honest sender's
    /// transactions are never rejected as stale, however the class lanes are interleaved. The
    /// same arrivals keyed by one shared stream are rejected, which is the defect removed.
    #[test]
    fn test_honest_sender_is_never_stale_under_any_cross_class_interleaving() {
        let lane_window = 8;
        let mut shared_stale = 0_usize;
        for seed in 0..200_u64 {
            let mut per_class: BTreeMap<MessageCategory, SequenceState> = BTreeMap::new();
            let mut shared: Option<SequenceState> = None;
            for (index, sent) in arrivals(seed, 100, lane_window).into_iter().enumerate() {
                let tag = u8::try_from(index % 251).expect("small tag");
                let (next, verdict) = observe(
                    per_class.remove(&sent.class),
                    sent.class_sequence,
                    digest(tag),
                );
                assert!(
                    verdict.permits_dispatch(),
                    "seed {seed}: {verdict:?} for class sequence {}",
                    sent.class_sequence
                );
                per_class.insert(sent.class, next);

                let (next, verdict) = observe(shared.take(), sent.shared_sequence, digest(tag));
                shared_stale += usize::from(matches!(verdict, SequenceVerdict::Stale { .. }));
                shared = Some(next);
            }
        }
        assert!(
            shared_stale > 0,
            "the shared stream must exhibit the #898 defect"
        );
    }

    #[test]
    fn transition_is_deterministic_for_the_same_state_and_input() {
        let (state, _) = observe(None, 4, digest(1));
        assert_eq!(
            observe(Some(state.clone()), 7, digest(2)),
            observe(Some(state), 7, digest(2))
        );
    }

    #[test]
    fn snapshot_encoding_preserves_structured_keys_and_full_width_sequences() {
        let origin: Did = SecretKey::random().address().into();
        let destination: Did = SecretKey::random().address().into();
        let key = StreamKey::new(7, origin, destination, MessageCategory::Application);
        let mut snapshot = ReplaySnapshot::default();
        snapshot.sender.insert(key, u64::MAX);
        snapshot
            .receiver
            .insert(key, SequenceState::first(u64::MAX, digest(9)));

        let encoded = rings_codec::serialize(&snapshot).expect("snapshot encodes");
        let decoded: ReplaySnapshot = rings_codec::deserialize(&encoded).expect("snapshot decodes");

        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn destinations_advance_independently() {
        let origin: Did = SecretKey::random().address().into();
        let a: Did = SecretKey::random().address().into();
        let b: Did = SecretKey::random().address().into();
        let mut states = BTreeMap::new();
        for (destination, sequence) in [(a, 8), (b, 0), (a, 10), (b, 1)] {
            let key = StreamKey::new(1, origin, destination, MessageCategory::Application);
            let previous = states.remove(&key);
            let (next, verdict) = observe(previous, sequence, digest(sequence as u8));
            assert!(verdict.permits_dispatch());
            states.insert(key, next);
        }
        assert_eq!(
            states
                .get(&StreamKey::new(1, origin, a, MessageCategory::Application))
                .map(SequenceState::high),
            Some(10)
        );
        assert_eq!(
            states
                .get(&StreamKey::new(1, origin, b, MessageCategory::Application))
                .map(SequenceState::high),
            Some(1)
        );
    }

    #[test]
    fn every_slot_in_the_bounded_reordering_window_is_admitted_once() {
        let high = TRANSACTION_REPLAY_BACKTRACK.saturating_add(50);
        let (mut state, verdict) = observe(None, high, digest(0));
        assert_eq!(verdict, SequenceVerdict::First);
        for sequence in state.retained_min()..high {
            let (next, verdict) = observe(Some(state), sequence, digest(sequence as u8));
            assert_eq!(verdict, SequenceVerdict::Late);
            state = next;
        }
        let below = state.retained_min().saturating_sub(1);
        let (_, verdict) = observe(Some(state), below, digest(255));
        assert_eq!(verdict, SequenceVerdict::Stale {
            retained_min: below.saturating_add(1),
        });
    }

    #[test]
    fn session_rotation_preserves_the_account_destination_stream_key() -> Result<()> {
        let account = SecretKey::random();
        let first_session = crate::delegation::DelegateeKey::new_with_seckey(&account)?;
        let rotated_session = crate::delegation::DelegateeKey::new_with_seckey(&account)?;
        let destination: Did = SecretKey::random().address().into();
        let first = crate::message::Transaction::new(
            destination,
            uuid::Uuid::new_v4(),
            0,
            None,
            crate::message::Message::custom(b"first")?,
            crate::message::MessageSigner::new(&first_session, 7),
        )?;
        let rotated = crate::message::Transaction::new(
            destination,
            uuid::Uuid::new_v4(),
            1,
            None,
            crate::message::Message::custom(b"rotated")?,
            crate::message::MessageSigner::new(&rotated_session, 7),
        )?;

        assert_ne!(
            first_session.delegation().delegatee_did(),
            rotated_session.delegation().delegatee_did()
        );
        assert_eq!(first.stream_key(7)?, rotated.stream_key(7)?);
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn sender_allocators_are_independent_per_destination() -> Result<()> {
        let origin: Did = SecretKey::random().address().into();
        let a: Did = SecretKey::random().address().into();
        let b: Did = SecretKey::random().address().into();
        let runtime = TransactionReplay::new(Box::new(crate::storage::MemStorage::new()));
        let key_a = StreamKey::new(1, origin, a, MessageCategory::Application);
        let key_b = StreamKey::new(1, origin, b, MessageCategory::Application);

        assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 0..=0);
        assert_eq!(runtime.reserve(key_b, NonZeroU64::MIN).await?, 0..=0);
        assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 1..=1);
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn sender_and_receiver_state_survive_runtime_recreation() -> Result<()> {
        let destination: Did = SecretKey::random().address().into();
        let key = stream(destination);
        let storage = std::sync::Arc::new(crate::storage::MemStorage::new());
        let first_runtime = TransactionReplay::new(Box::new(SharedStorage(storage.clone())));
        assert_eq!(first_runtime.reserve(key, NonZeroU64::MIN).await?, 0..=0);
        assert_eq!(
            first_runtime.admit(key, 0, digest(1)).await?,
            SequenceVerdict::First
        );
        drop(first_runtime);

        let restarted = TransactionReplay::new(Box::new(SharedStorage(storage)));
        assert_eq!(restarted.reserve(key, NonZeroU64::MIN).await?, 1..=1);
        assert!(matches!(
            restarted.admit(key, 0, digest(1)).await,
            Err(Error::TransactionReplay { .. })
        ));
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn counter_exhaustion_fails_closed() -> Result<()> {
        let key = stream(SecretKey::random().address().into());
        let storage = crate::storage::MemStorage::new();
        let runtime = TransactionReplay::new(Box::new(storage));
        {
            let mut state = runtime.state.lock().await;
            let snapshot = runtime.load_snapshot(&mut state.snapshot).await?;
            snapshot.sender.insert(key, u64::MAX);
            runtime.persist(snapshot).await?;
        }
        assert!(matches!(
            runtime.reserve(key, NonZeroU64::MIN).await,
            Err(Error::TransactionSequenceExhausted { .. })
        ));
        Ok(())
    }

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn load_failure_fails_closed_and_is_counted() {
        let runtime = TransactionReplay::new(Box::new(FailingStorage));
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

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn store_failure_fails_closed_before_admission_and_is_counted() {
        let runtime = TransactionReplay::new(Box::new(StoreFailingStorage));
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

    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn new_sender_stream_fails_closed_at_the_table_bound() -> Result<()> {
        assert_eq!(TRANSACTION_REPLAY_STREAM_CAPACITY, 4096);
        let runtime = TransactionReplay::new(Box::new(crate::storage::MemStorage::new()));
        let destination = Did::from(1_u32);
        {
            let mut state = runtime.state.lock().await;
            let snapshot = runtime.load_snapshot(&mut state.snapshot).await?;
            for origin in 0_u32..4096 {
                snapshot.sender.insert(
                    StreamKey::new(
                        1,
                        Did::from(origin),
                        destination,
                        MessageCategory::Application,
                    ),
                    0,
                );
            }
            runtime.persist(snapshot).await?;
        }
        let new_key = StreamKey::new(
            1,
            Did::from(4097_u32),
            destination,
            MessageCategory::Application,
        );
        assert!(matches!(
            runtime.reserve(new_key, NonZeroU64::MIN).await,
            Err(Error::TransactionReplayStreamCapacityExceeded { capacity: 4096 })
        ));
        Ok(())
    }

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    #[wasm_bindgen_test::wasm_bindgen_test]
    async fn browser_storage_round_trip_retains_nonempty_replay_state() {
        const STORAGE_NAME: &str = "rings-core/replay-snapshot-round-trip";
        let storage = crate::storage::idb::IdbStorage::new_with_cap_and_name(2, STORAGE_NAME)
            .await
            .expect("IndexedDB opens");
        storage.clear().await.expect("IndexedDB clears");
        let origin: Did = SecretKey::random().address().into();
        let destination: Did = SecretKey::random().address().into();
        let key = StreamKey::new(7, origin, destination, MessageCategory::Application);
        let first = TransactionReplay::new(Box::new(storage));

        assert_eq!(
            first
                .reserve(key, NonZeroU64::MIN)
                .await
                .expect("sender reservation persists"),
            0..=0
        );
        assert_eq!(
            first
                .admit(key, u64::MAX, digest(1))
                .await
                .expect("receiver state persists"),
            SequenceVerdict::First
        );
        drop(first);

        let reopened = crate::storage::idb::IdbStorage::new_with_cap_and_name(2, STORAGE_NAME)
            .await
            .expect("IndexedDB reopens");
        let restarted = TransactionReplay::new(Box::new(reopened));
        assert_eq!(
            restarted
                .reserve(key, NonZeroU64::MIN)
                .await
                .expect("sender state reloads"),
            1..=1
        );
        assert!(matches!(
            restarted.admit(key, u64::MAX, digest(1)).await,
            Err(Error::TransactionReplay { .. })
        ));
    }

    #[cfg(not(target_family = "wasm"))]
    struct FailingStorage;

    #[cfg(not(target_family = "wasm"))]
    #[async_trait::async_trait]
    impl KvStorageInterface<ReplaySnapshot> for FailingStorage {
        async fn get(&self, _key: &str) -> Result<Option<ReplaySnapshot>> {
            Err(Error::InvalidTransport)
        }

        async fn put(&self, _key: &str, _value: &ReplaySnapshot) -> Result<()> {
            Err(Error::InvalidTransport)
        }

        async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
            Err(Error::InvalidTransport)
        }

        // Only reads fail: this storage models a snapshot that cannot be loaded.
        async fn remove(&self, _key: &str) -> Result<()> {
            Ok(())
        }

        async fn clear(&self) -> Result<()> {
            Err(Error::InvalidTransport)
        }

        async fn count(&self) -> Result<u32> {
            Err(Error::InvalidTransport)
        }
    }

    #[cfg(not(target_family = "wasm"))]
    struct StoreFailingStorage;

    #[cfg(not(target_family = "wasm"))]
    #[async_trait::async_trait]
    impl KvStorageInterface<ReplaySnapshot> for StoreFailingStorage {
        async fn get(&self, _key: &str) -> Result<Option<ReplaySnapshot>> {
            Ok(None)
        }

        async fn put(&self, _key: &str, _value: &ReplaySnapshot) -> Result<()> {
            Err(Error::InvalidTransport)
        }

        async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
            Ok(Vec::new())
        }

        async fn remove(&self, _key: &str) -> Result<()> {
            Ok(())
        }

        async fn clear(&self) -> Result<()> {
            Ok(())
        }

        async fn count(&self) -> Result<u32> {
            Ok(0)
        }
    }

    /// A store that still holds a shared-stream snapshot under the key used before #898.
    /// Reading that key is an error, so a passing test proves the cutover never reads it.
    #[cfg(not(target_family = "wasm"))]
    struct CutoverStorage {
        /// The stored records.
        inner: crate::storage::MemStorage<ReplaySnapshot>,
        /// Whether removing a record fails.
        fail_remove: bool,
    }

    #[cfg(not(target_family = "wasm"))]
    impl CutoverStorage {
        /// A store holding a snapshot under the shared-stream key.
        async fn holding_a_shared_stream_snapshot(fail_remove: bool) -> Result<Self> {
            let inner = crate::storage::MemStorage::new();
            inner
                .put(SHARED_STREAM_SNAPSHOT_KEY, &ReplaySnapshot::default())
                .await?;
            Ok(Self { inner, fail_remove })
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[async_trait::async_trait]
    impl KvStorageInterface<ReplaySnapshot> for CutoverStorage {
        async fn get(&self, key: &str) -> Result<Option<ReplaySnapshot>> {
            if key == SHARED_STREAM_SNAPSHOT_KEY {
                return Err(Error::InvalidTransport);
            }
            self.inner.get(key).await
        }

        async fn put(&self, key: &str, value: &ReplaySnapshot) -> Result<()> {
            self.inner.put(key, value).await
        }

        async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
            self.inner.get_all().await
        }

        async fn remove(&self, key: &str) -> Result<()> {
            if self.fail_remove {
                return Err(Error::InvalidTransport);
            }
            self.inner.remove(key).await
        }

        async fn clear(&self) -> Result<()> {
            self.inner.clear().await
        }

        async fn count(&self) -> Result<u32> {
            self.inner.count().await
        }
    }

    /// Cutover of #898: the first load deletes the shared-stream snapshot without reading it,
    /// and admission proceeds on fresh per-class streams.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn test_first_load_deletes_the_shared_stream_snapshot_unread() -> Result<()> {
        let storage =
            std::sync::Arc::new(CutoverStorage::holding_a_shared_stream_snapshot(false).await?);
        let runtime = TransactionReplay::new(Box::new(SharedCutoverStorage(storage.clone())));
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
            .get(TRANSACTION_REPLAY_SNAPSHOT_KEY)
            .await?
            .is_some());
        assert_eq!(runtime.counters().persistence_failure, 0);
        Ok(())
    }

    /// A failed deletion of the shared-stream snapshot fails closed and is counted; the next
    /// load retries it.
    #[cfg(not(target_family = "wasm"))]
    #[tokio::test]
    async fn test_failed_shared_stream_deletion_fails_closed_and_is_counted() -> Result<()> {
        let storage = CutoverStorage::holding_a_shared_stream_snapshot(true).await?;
        let runtime = TransactionReplay::new(Box::new(storage));
        let key = stream(SecretKey::random().address().into());

        assert!(matches!(
            runtime.admit(key, 0, digest(1)).await,
            Err(Error::TransactionReplayPersistence {
                operation: "remove shared-stream snapshot",
                ..
            })
        ));
        assert_eq!(runtime.counters().persistence_failure, 1);
        Ok(())
    }

    /// Shares one [`CutoverStorage`] between the runtime and the test's assertions.
    #[cfg(not(target_family = "wasm"))]
    struct SharedCutoverStorage(std::sync::Arc<CutoverStorage>);

    #[cfg(not(target_family = "wasm"))]
    #[async_trait::async_trait]
    impl KvStorageInterface<ReplaySnapshot> for SharedCutoverStorage {
        async fn get(&self, key: &str) -> Result<Option<ReplaySnapshot>> {
            self.0.get(key).await
        }

        async fn put(&self, key: &str, value: &ReplaySnapshot) -> Result<()> {
            self.0.put(key, value).await
        }

        async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
            self.0.get_all().await
        }

        async fn remove(&self, key: &str) -> Result<()> {
            self.0.remove(key).await
        }

        async fn clear(&self) -> Result<()> {
            self.0.clear().await
        }

        async fn count(&self) -> Result<u32> {
            self.0.count().await
        }
    }

    #[cfg(not(target_family = "wasm"))]
    struct SharedStorage(std::sync::Arc<crate::storage::MemStorage<ReplaySnapshot>>);

    #[cfg(not(target_family = "wasm"))]
    #[async_trait::async_trait]
    impl KvStorageInterface<ReplaySnapshot> for SharedStorage {
        async fn get(&self, key: &str) -> Result<Option<ReplaySnapshot>> {
            self.0.get(key).await
        }

        async fn put(&self, key: &str, value: &ReplaySnapshot) -> Result<()> {
            self.0.put(key, value).await
        }

        async fn get_all(&self) -> Result<Vec<(String, ReplaySnapshot)>> {
            self.0.get_all().await
        }

        async fn remove(&self, key: &str) -> Result<()> {
            self.0.remove(key).await
        }

        async fn clear(&self) -> Result<()> {
            self.0.clear().await
        }

        async fn count(&self) -> Result<u32> {
            self.0.count().await
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod quota_admission_tests;
