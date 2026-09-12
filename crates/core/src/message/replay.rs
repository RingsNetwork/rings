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
//! replay store. Deleting that store deletes the corresponding replay guarantee.

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
use crate::storage::KvStorageInterface;

/// Number of out-of-order sequence slots retained for one destination-scoped stream.
pub const TRANSACTION_REPLAY_WINDOW: usize = 32;
const TRANSACTION_REPLAY_WINDOW_U64: u64 = 32;
const TRANSACTION_REPLAY_BACKTRACK: u64 = 31;
/// Maximum sender streams and maximum receiver streams retained by one runtime.
pub const TRANSACTION_REPLAY_STREAM_CAPACITY: usize = 4096;
const TRANSACTION_REPLAY_SNAPSHOT_KEY: &str = "rings-core:transaction-replay:v2";

/// A destination-scoped transaction stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct StreamKey {
    /// Overlay in which the transaction signature is valid.
    pub network_id: u32,
    /// Account DID recovered from the transaction's delegated session.
    pub origin_account: Did,
    /// Final logical destination of the transaction.
    pub destination: Did,
}

impl StreamKey {
    /// Name one account-to-destination stream inside an overlay.
    pub const fn new(network_id: u32, origin_account: Did, destination: Did) -> Self {
        Self {
            network_id,
            origin_account,
            destination,
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
/// Fields are private so only [`TransactionReplay`] can apply the capacity and persistence laws.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReplaySnapshot {
    sender: BTreeMap<StreamKey, u64>,
    receiver: BTreeMap<StreamKey, SequenceState>,
}

/// Storage accepted by the transaction replay runtime.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type ReplayStorage = Box<dyn KvStorageInterface<ReplaySnapshot>>;

/// Storage accepted by the transaction replay runtime.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type ReplayStorage = Box<dyn KvStorageInterface<ReplaySnapshot> + Send + Sync>;

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
    snapshot: Mutex<Option<ReplaySnapshot>>,
    counters: ReplayCounterState,
}

impl TransactionReplay {
    /// Construct a replay runtime. The snapshot is loaded lazily on its first operation.
    pub(crate) fn new(storage: ReplayStorage) -> Self {
        Self {
            storage,
            snapshot: Mutex::new(None),
            counters: ReplayCounterState::default(),
        }
    }

    /// Current observable counters.
    pub(crate) fn counters(&self) -> ReplayCounters {
        self.counters.snapshot()
    }

    async fn load_snapshot<'a>(
        &self,
        slot: &'a mut Option<ReplaySnapshot>,
    ) -> Result<&'a mut ReplaySnapshot> {
        if slot.is_none() {
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
        let mut slot = self.snapshot.lock().await;
        let snapshot = self.load_snapshot(&mut slot).await?;
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

    /// Persist one receiver transition before allowing final-destination dispatch.
    pub(crate) async fn admit(
        &self,
        key: StreamKey,
        sequence: u64,
        digest: TransactionDigest,
    ) -> Result<SequenceVerdict> {
        let mut slot = self.snapshot.lock().await;
        let snapshot = self.load_snapshot(&mut slot).await?;
        let previous = snapshot.receiver.get(&key).cloned();
        if previous.is_none() && snapshot.receiver.len() >= TRANSACTION_REPLAY_STREAM_CAPACITY {
            return Err(Error::TransactionReplayStreamCapacityExceeded {
                capacity: TRANSACTION_REPLAY_STREAM_CAPACITY,
            });
        }
        let (next, verdict) = observe(previous.clone(), sequence, digest);
        match verdict {
            SequenceVerdict::First | SequenceVerdict::Advance | SequenceVerdict::Late => {
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
                    return Err(error);
                }
                Ok(verdict)
            }
            SequenceVerdict::Replay => {
                self.counters.replay.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionReplay { key, sequence })
            }
            SequenceVerdict::Fork { accepted, incoming } => {
                self.counters.fork.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionSequenceFork {
                    key,
                    sequence,
                    evidence: Box::new(TransactionForkEvidence { accepted, incoming }),
                })
            }
            SequenceVerdict::Stale { retained_min } => {
                self.counters.stale.fetch_add(1, Ordering::Relaxed);
                Err(Error::TransactionSequenceStale {
                    key,
                    sequence,
                    retained_min,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::SecretKey;

    fn digest(value: u8) -> TransactionDigest {
        TransactionDigest::new([value; 32])
    }

    fn stream(destination: Did) -> StreamKey {
        StreamKey::new(7, SecretKey::random().address().into(), destination)
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

    #[test]
    fn transition_is_deterministic_for_the_same_state_and_input() {
        let (state, _) = observe(None, 4, digest(1));
        assert_eq!(
            observe(Some(state.clone()), 7, digest(2)),
            observe(Some(state), 7, digest(2))
        );
    }

    #[test]
    fn destinations_advance_independently() {
        let origin: Did = SecretKey::random().address().into();
        let a: Did = SecretKey::random().address().into();
        let b: Did = SecretKey::random().address().into();
        let mut states = BTreeMap::new();
        for (destination, sequence) in [(a, 8), (b, 0), (a, 10), (b, 1)] {
            let key = StreamKey::new(1, origin, destination);
            let previous = states.remove(&key);
            let (next, verdict) = observe(previous, sequence, digest(sequence as u8));
            assert!(verdict.permits_dispatch());
            states.insert(key, next);
        }
        assert_eq!(
            states
                .get(&StreamKey::new(1, origin, a))
                .map(SequenceState::high),
            Some(10)
        );
        assert_eq!(
            states
                .get(&StreamKey::new(1, origin, b))
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
        let first_session = crate::session::SessionSk::new_with_seckey(&account)?;
        let rotated_session = crate::session::SessionSk::new_with_seckey(&account)?;
        let destination: Did = SecretKey::random().address().into();
        let first = crate::message::Transaction::new(
            destination,
            uuid::Uuid::new_v4(),
            0,
            crate::message::Message::custom(b"first")?,
            crate::message::MessageSigner::new(&first_session, 7),
        )?;
        let rotated = crate::message::Transaction::new(
            destination,
            uuid::Uuid::new_v4(),
            1,
            crate::message::Message::custom(b"rotated")?,
            crate::message::MessageSigner::new(&rotated_session, 7),
        )?;

        assert_ne!(
            first_session.session().session_did(),
            rotated_session.session().session_did()
        );
        assert_eq!(first.stream_key(7), rotated.stream_key(7));
        Ok(())
    }

    #[tokio::test]
    async fn sender_allocators_are_independent_per_destination() -> Result<()> {
        let origin: Did = SecretKey::random().address().into();
        let a: Did = SecretKey::random().address().into();
        let b: Did = SecretKey::random().address().into();
        let runtime = TransactionReplay::new(Box::new(crate::storage::MemStorage::new()));
        let key_a = StreamKey::new(1, origin, a);
        let key_b = StreamKey::new(1, origin, b);

        assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 0..=0);
        assert_eq!(runtime.reserve(key_b, NonZeroU64::MIN).await?, 0..=0);
        assert_eq!(runtime.reserve(key_a, NonZeroU64::MIN).await?, 1..=1);
        Ok(())
    }

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

    #[tokio::test]
    async fn counter_exhaustion_fails_closed() -> Result<()> {
        let key = stream(SecretKey::random().address().into());
        let storage = crate::storage::MemStorage::new();
        let runtime = TransactionReplay::new(Box::new(storage));
        {
            let mut slot = runtime.snapshot.lock().await;
            let snapshot = runtime.load_snapshot(&mut slot).await?;
            snapshot.sender.insert(key, u64::MAX);
            runtime.persist(snapshot).await?;
        }
        assert!(matches!(
            runtime.reserve(key, NonZeroU64::MIN).await,
            Err(Error::TransactionSequenceExhausted { .. })
        ));
        Ok(())
    }

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

    #[tokio::test]
    async fn new_sender_stream_fails_closed_at_the_table_bound() -> Result<()> {
        assert_eq!(TRANSACTION_REPLAY_STREAM_CAPACITY, 4096);
        let runtime = TransactionReplay::new(Box::new(crate::storage::MemStorage::new()));
        let destination = Did::from(1_u32);
        {
            let mut slot = runtime.snapshot.lock().await;
            let snapshot = runtime.load_snapshot(&mut slot).await?;
            for origin in 0_u32..4096 {
                snapshot
                    .sender
                    .insert(StreamKey::new(1, Did::from(origin), destination), 0);
            }
            runtime.persist(snapshot).await?;
        }
        let new_key = StreamKey::new(1, Did::from(4097_u32), destination);
        assert!(matches!(
            runtime.reserve(new_key, NonZeroU64::MIN).await,
            Err(Error::TransactionReplayStreamCapacityExceeded { capacity: 4096 })
        ));
        Ok(())
    }

    struct FailingStorage;

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

        async fn remove(&self, _key: &str) -> Result<()> {
            Err(Error::InvalidTransport)
        }

        async fn clear(&self) -> Result<()> {
            Err(Error::InvalidTransport)
        }

        async fn count(&self) -> Result<u32> {
            Err(Error::InvalidTransport)
        }
    }

    struct StoreFailingStorage;

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

    struct SharedStorage(std::sync::Arc<crate::storage::MemStorage<ReplaySnapshot>>);

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
