//! DHT types about `Storage` and `PeerRing`.
#![deny(missing_docs)]
use async_trait::async_trait;

use super::did::Did;
use super::entry::Entry;
use super::entry::PlacementMiss;
use super::entry::SyncedEntryAck;
use crate::error::Result;

/// Chord is a distributed hash table (DHT) algorithm that is designed to efficiently
/// distribute data across peer-to-peer network nodes. You may want to browse its
/// [wiki](https://en.wikipedia.org/wiki/Chord_(peer-to-peer)) before you read this.
///
/// A basic usage of Chord in rings network is to assist the nodes in passing messages
/// so that they can forward data with fewer connections. In this situation, the key
/// of Chord is the unique identifier of a node, which we call [Did]. Then if we connect
/// all the nodes in the finger table for every node, we construct a [PeerRing](super::PeerRing).
/// It's the basic construction of the rings network. When passing a message to a
/// destination node, each hop forwards to its closest known peer on `(self, destination]`,
/// and a hop that knows none hands the message once, terminally, to the first linked node after it
/// (see [`delivery`](super::delivery)); `find_successor` answers the different question of
/// who owns a ring position. A route takes O(log n) hops over O(log n) connections per node
/// on a converged ring.
///
/// Some methods return an `Action` which is used to tell outer the extra action to take
/// after handling data inside the struct. It's useful since the struct may not work
/// for managing whole data but for giving strategies by data inside.
pub trait Chord<Action> {
    /// Ask DHT for the successor of Did.
    /// May return a remote action for the successor is recorded in another node.
    fn find_successor(&self, did: Did) -> Result<Action>;

    /// Notify the DHT that a node is its predecessor.
    /// According to the paper, this method should be called periodically.
    /// This method should return the predecessor after updating.
    fn notify(&self, did: Did) -> Result<Did>;

    /// Fix finger table by finding the successor for each finger.
    /// According to the paper, this method should be called periodically.
    /// According to the paper, only one finger should be fixed at a time.
    fn fix_fingers(&self) -> Result<Action>;
}

/// ChordStorageSync defines storage hand-off triggered by ownership changes.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageSync<Action>: Chord<Action> {
    /// Offer the live `Entry`s no longer placed in `(self, new_successor]` to
    /// `new_successor`. The storage repair pass runs this against the current
    /// successor head, whichever input moved it, so repetition must be idempotent.
    ///
    /// Mode law: a relay inbox is placed by the ring geometry in every mode and
    /// is always offered to `new_successor`. With storage virtual nodes enabled a
    /// data topic follows the current `storage_owner(k, view, cfg)` relation
    /// instead, and `new_successor` is only the trigger of that additive copy.
    ///
    /// Post: this only delivers entry joins. Local cleanup is performed by
    /// [`Self::acknowledge_synced_entries`] after the successor reports durable
    /// storage for specific placement keys.
    async fn sync_entries_with_successor(&self, new_successor: Did) -> Result<Action>;

    /// Delete local entries whose placement keys and exact values were durably
    /// stored by the successor during sync.
    ///
    /// Post S2': only keys present in `acks` may be removed, and a key is
    /// removed only if its current local value equals the value carried by the
    /// corresponding ack.
    async fn acknowledge_synced_entries(&self, acks: &[SyncedEntryAck]) -> Result<Action>;
}

/// ChordStorageRepair defines additive repair for redundant DHT storage.
///
/// Repair never deletes a live local copy. It only republishes a known
/// [`Entry`] as a join delivery to the current affine placement set so missing
/// owners can regain a copy; values whose retention bound has elapsed are
/// retired before republish and are never offered.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageRepair<Action>: Chord<Action> {
    /// Republish every live locally stored entry to its current affine owners.
    ///
    /// Post: no live local key is removed. Remote actions, if any, are
    /// join-delivery sync messages carrying explicit placement keys.
    async fn republish_local_entries(&self, redundancy: u16) -> Result<Action>;

    /// Copy a found entry only to placement keys observed missing during lookup.
    ///
    /// Post: `misses.is_empty()` is a no-op. Non-empty repair emits copy-only
    /// actions for exactly the observed misses and performs no additional
    /// placement probing.
    async fn read_repair_entry(
        &self,
        entry: Entry,
        misses: &[PlacementMiss],
        redundancy: u16,
    ) -> Result<Action>;
}

/// ChordStorageCache defines the basic API for getting and setting DHT cache storage.
///
/// The cache is bounded and shares the storage admission law: a fetched entry
/// is cached only if it could have been accepted into storage, and it is
/// retired once its retention bound elapses.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait ChordStorageCache<Action>: Chord<Action> {
    /// Cache fetched resource locally.
    async fn local_cache_put(&self, entry: Entry) -> Result<()>;
    /// Get a live cached entry.
    async fn local_cache_get(&self, entry_key: Did) -> Result<Option<Entry>>;
}
