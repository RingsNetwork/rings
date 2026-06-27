//! CRDT carriers for DHT entries.
//!
//! State variables:
//! - `register` is an optional LWW reset floor for overwrite.
//! - `values` is an LWW element set keyed by encoded payload.
//! - `removes` is a two-phase tombstone set for relay-message entries.
//!
//! Semilattice laws:
//! - `GSet` join is set union.
//! - `DataTopicBuffer` join is idempotent, commutative, and associative over
//!   normalized LWW element sets.
//! - `RelayMessageSet` join is idempotent, commutative, and associative over
//!   normalized two-phase sets.
//!
//! Constructor postconditions:
//! - `DataTopicBuffer::new` preserves only values whose dot is at or after the
//!   reset floor when a reset floor exists.
//! - `RelayMessageSet::new` preserves only adds whose dot has not been
//!   tombstoned.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use crate::algebra::JoinSemilattice;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;

/// Hybrid logical version for LWW entry registers and element dots.
///
/// `logical_time_ms` starts from the wall-clock millisecond observed at the
/// storage-operation boundary, then advances beyond any local floor that would
/// otherwise dominate it. `actor` and `operation` make concurrent writes from
/// the same millisecond totally ordered without claiming wall-clock recency.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryVersion {
    /// Hybrid logical time in milliseconds.
    #[serde(alias = "epoch_ms")]
    pub logical_time_ms: u128,
    /// Storage node that first stamped the operation.
    pub actor: Did,
    /// Deterministic digest of the stamped operation payload.
    #[serde(default)]
    pub operation: Did,
}

impl EntryVersion {
    /// Construct a version from an explicit hybrid logical time and actor.
    pub fn new(logical_time_ms: u128, actor: Did, operation: Did) -> Self {
        Self {
            logical_time_ms,
            actor,
            operation,
        }
    }

    /// Construct a version at the current operation boundary.
    pub fn issued_by(actor: Did, operation: Did) -> Self {
        Self::new(crate::utils::get_epoch_ms(), actor, operation)
    }

    pub(super) fn after(self, floor: Option<Self>) -> Self {
        let Some(floor) = floor else {
            return self;
        };
        if self > floor {
            return self;
        }
        Self {
            logical_time_ms: floor.logical_time_ms.saturating_add(1),
            actor: self.actor,
            operation: self.operation,
        }
    }
}

/// Unique add witness for one visible entry payload element.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryDot {
    /// LWW version that issued this element.
    pub version: EntryVersion,
    /// Element position inside the issuing operation.
    pub index: u32,
}

impl EntryDot {
    pub(super) fn for_index(version: EntryVersion, index: usize) -> Result<Self> {
        let index = u32::try_from(index).map_err(|_| Error::EntryDotIndexOutOfBounds { index })?;
        Ok(Self { version, index })
    }
}

/// CRDT metadata carried beside the legacy entry payload.
///
/// `register` is the LWW reset floor used by overwrite. `dots` are per-element
/// add witnesses used by data/topic and relay element sets. `tombstones` is the
/// remove set for relay-message two-phase semantics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryCrdt {
    /// Optional LWW reset floor for the entry payload.
    pub register: Option<EntryVersion>,
    /// Per-element add dots. When absent, legacy entries synthesize dots from
    /// their payload order and value digest.
    pub dots: Vec<EntryDot>,
    /// Remove dots for two-phase sets.
    pub tombstones: Vec<EntryDot>,
}

impl EntryCrdt {
    pub(super) fn has_write_witness(&self) -> bool {
        self.register.is_some() || !self.dots.is_empty()
    }

    /// Return the bottom floor used only to lift legacy payloads without dots.
    pub(super) fn legacy_floor(&self) -> EntryVersion {
        self.register.unwrap_or_default()
    }
}

/// Grow-only set used by subring membership.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GSet<T: Ord> {
    members: BTreeSet<T>,
}

impl<T: Ord> GSet<T> {
    /// Construct an empty grow-only set.
    pub fn new() -> Self {
        Self {
            members: BTreeSet::new(),
        }
    }

    /// Insert one member.
    pub fn insert(&mut self, member: T) {
        self.members.insert(member);
    }

    /// Iterate over members in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.members.iter()
    }
}

impl<T: Ord> JoinSemilattice for GSet<T> {
    fn join(mut self, other: Self) -> Self {
        self.members.extend(other.members);
        self
    }
}

/// Bounded LWW element set used by data topic buffers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataTopicBuffer {
    pub(super) register: Option<EntryVersion>,
    pub(super) values: BTreeMap<Encoded, EntryDot>,
}

impl DataTopicBuffer {
    pub(super) fn new(
        register: Option<EntryVersion>,
        mut values: BTreeMap<Encoded, EntryDot>,
    ) -> Self {
        if let Some(floor) = register {
            values.retain(|_, dot| dot.version >= floor);
        }
        Self { register, values }
    }
}

impl JoinSemilattice for DataTopicBuffer {
    fn join(mut self, other: Self) -> Self {
        self.register = self.register.max(other.register);
        for (value, dot) in other.values {
            self.values
                .entry(value)
                .and_modify(|current| *current = (*current).max(dot))
                .or_insert(dot);
        }
        Self::new(self.register, self.values)
    }
}

/// Two-phase set used by relay-message storage.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayMessageSet {
    pub(super) adds: DataTopicBuffer,
    pub(super) removes: BTreeSet<EntryDot>,
}

impl RelayMessageSet {
    pub(super) fn new(mut adds: DataTopicBuffer, removes: BTreeSet<EntryDot>) -> Self {
        adds.values.retain(|_, dot| !removes.contains(dot));
        Self { adds, removes }
    }
}

impl JoinSemilattice for RelayMessageSet {
    fn join(mut self, other: Self) -> Self {
        self.adds = self.adds.join(other.adds);
        self.removes.extend(other.removes);
        Self::new(self.adds, self.removes)
    }
}

/// Grow-only subring membership CRDT.
pub type SubringMemberSet = GSet<Did>;
