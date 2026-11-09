//! Data-topic write provenance and fair per-writer element retention (#1179).
//!
//! Write operations and replicated sync states on [`EntryKind::Data`](super::EntryKind::Data)
//! carriers are classified through algebraic sum types ([`DataProvenance`], [`RetractionWitness`])
//! and higher-order continuation combinators (`with_live_and_then`, `and_then`, `then_some`),
//! isolating per-actor provenance proofs from storage transitions.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bytes::Bytes;

use super::ElementDigest;
use super::Entry;
use super::EntryDot;
use super::EntryOperation;
use super::EntryVersion;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Principal;

/// The selector by which a [`EntryOperation::Tombstone`] names the elements to retract from a
/// carrier: either explicit CRDT add dots (`Dots`) when a write witness is present, or raw payload
/// values (`Values`) otherwise.
pub(super) enum RetractionWitness<'a> {
    /// Retract only elements held at one of the specified add dots.
    Dots(BTreeSet<EntryDot>),
    /// Retract elements whose payload bytes match one of the specified values.
    Values(BTreeSet<&'a Bytes>),
}

impl<'a> RetractionWitness<'a> {
    /// Construct the retraction witness carried by `entry`.
    pub(super) fn of(entry: &'a Entry) -> Self {
        entry
            .crdt
            .has_write_witness()
            .then(|| {
                (!entry.crdt.dots.is_empty())
                    .then(|| Self::Dots(entry.crdt.dots.iter().copied().collect()))
            })
            .flatten()
            .unwrap_or_else(|| Self::Values(entry.data.iter().collect()))
    }

    /// Whether the held element `(value, dot)` is selected by this witness.
    pub(super) fn matches(&self, value: &Bytes, dot: &EntryDot) -> bool {
        match self {
            Self::Dots(dots) => dots.contains(dot),
            Self::Values(values) => values.contains(value),
        }
    }
}

/// Algebraic classification of a verified data-topic write claim issued by `writer`.
enum DataProvenance<'a> {
    /// Full-carrier reset (`Overwrite`) authored by `writer`.
    Reset { entry: &'a Entry, writer: Did },
    /// Element-set append (`Extend`) authored by `writer`.
    Append { entry: &'a Entry, writer: Did },
    /// Element retraction (`Tombstone`) authored by `writer` under `witness`.
    Retract {
        witness: RetractionWitness<'a>,
        writer: Did,
    },
}

impl<'a> DataProvenance<'a> {
    /// Construct a provenance proof for `(op, writer, introducer)`, or reject with
    /// [`Error::DataEntryWriteUnauthorized`].
    fn try_new(op: &'a EntryOperation, writer: Did, introducer: Principal) -> Result<Self> {
        op.entry()
            .is_exclusively_authored_by(writer)
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
            .and_then(|()| match op {
                EntryOperation::Overwrite(entry) => entry
                    .permits_reset_envelope_from(introducer)
                    .then_some(Self::Reset { entry, writer })
                    .ok_or(Error::DataEntryWriteUnauthorized),
                EntryOperation::Extend(entry) => entry
                    .has_delta_envelope()
                    .then_some(Self::Append { entry, writer })
                    .ok_or(Error::DataEntryWriteUnauthorized),
                EntryOperation::Tombstone(entry) => entry
                    .has_delta_envelope()
                    .then(|| Self::Retract {
                        witness: RetractionWitness::of(entry),
                        writer,
                    })
                    .ok_or(Error::DataEntryWriteUnauthorized),
            })
    }

    /// Construct a provenance claim for validating `op` against a live local carrier.
    fn for_local(op: &'a EntryOperation, writer: Did) -> Self {
        match op {
            EntryOperation::Overwrite(entry) => Self::Reset { entry, writer },
            EntryOperation::Extend(entry) => Self::Append { entry, writer },
            EntryOperation::Tombstone(entry) => Self::Retract {
                witness: RetractionWitness::of(entry),
                writer,
            },
        }
    }

    /// Verify that this write claim may be applied to `live`.
    fn verify_on_live(&self, live: &Entry) -> Result<()> {
        match self {
            Self::Reset { entry, writer } => live.permits_reset_by(entry, *writer),
            Self::Append { entry, writer } => live.permits_append_by(entry, *writer),
            Self::Retract { witness, writer } => live.permits_retraction_by(witness, *writer),
        }
    }
}

impl EntryVersion {
    /// Whether this version is either unattributed (`actor.is_default()`) or authored by `writer`.
    pub(super) fn is_authored_by(self, writer: Did) -> bool {
        self.actor.is_default() || self.actor == writer
    }
}

impl EntryDot {
    /// Whether this dot carries a non-default author identity (`!version.actor.is_default()`).
    pub(super) fn is_attributed(self) -> bool {
        !self.version.actor.is_default()
    }

    /// Return the attributed author of this dot, or `None` when unattributed.
    fn attributed_actor(self) -> Option<Did> {
        self.is_attributed().then_some(self.version.actor)
    }
}

impl EntryOperation {
    /// Validate a data-topic write operation `self` issued by `writer` and introduced by
    /// `introducer` at `now_ms` inside `network_id` (#1179).
    ///
    /// Enforces:
    /// - Bounds and liveness (`validate_admissible_at`).
    /// - Provenance construction ([`DataProvenance::try_new`]): every non-default version carried
    ///   by the operation was authored by `writer`, `Overwrite` carries no raw tombstones and is
    ///   non-empty when remote, and `Extend`/`Tombstone` carry neither a reset register nor raw
    ///   tombstones.
    pub(crate) fn validate_data_write(
        &self,
        now_ms: u128,
        network_id: u32,
        writer: Did,
        introducer: Principal,
    ) -> Result<()> {
        self.validate_admissible_at(now_ms, network_id)
            .and_then(|()| DataProvenance::try_new(self, writer, introducer).map(drop))
    }

    /// Validate a data-topic operation `self` issued by `writer` against the live carrier
    /// `local` currently held at the storage slot (#1179).
    ///
    /// - `Overwrite`: rejected when `entry.data` is empty against a live carrier, or when `local`
    ///   holds any live element, remove, or reset floor authored by another writer.
    /// - `Tombstone`: rejected when any matched element in `local` (by [`RetractionWitness`]) was
    ///   authored by another writer.
    pub(crate) fn validate_data_write_on_local(
        &self,
        local: Option<&Entry>,
        writer: Did,
    ) -> Result<()> {
        Entry::with_live_and_then(local, |live| {
            DataProvenance::for_local(self, writer).verify_on_live(live)
        })
    }
}

impl Entry {
    /// Evaluate `then` on `local` when a live carrier is present, or succeed with `Ok(())` when the
    /// slot is empty.
    pub(crate) fn with_live_and_then(
        local: Option<&Self>,
        then: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        local.map_or(Ok(()), then)
    }

    /// Whether every version carried by `self` is unattributed or authored by `writer`.
    pub(super) fn is_exclusively_authored_by(&self, writer: Did) -> bool {
        self.versions()
            .all(|version| version.is_authored_by(writer))
    }

    /// Whether an `Overwrite` carrier satisfies the reset envelope invariant for `introducer`:
    /// carries no raw tombstones and, when introduced by a remote peer, is non-empty.
    fn permits_reset_envelope_from(&self, introducer: Principal) -> bool {
        self.crdt.tombstones.is_empty() && (!self.data.is_empty() || introducer == Principal::Local)
    }

    /// Whether an `Extend` or `Tombstone` carrier satisfies the delta envelope invariant: carries
    /// neither a reset register nor raw tombstones.
    fn has_delta_envelope(&self) -> bool {
        self.crdt.register.is_none() && self.crdt.tombstones.is_empty()
    }

    /// Verify that `incoming` may reset this live carrier on behalf of `writer`.
    fn permits_reset_by(&self, incoming: &Self, writer: Did) -> Result<()> {
        (!incoming.data.is_empty() && self.is_exclusively_authored_by(writer))
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
    }

    /// Verify that `incoming` does not overwrite any live element in `self` authored by a peer
    /// other than `writer`.
    fn permits_append_by(&self, incoming: &Self, writer: Did) -> Result<()> {
        self.topic_buffer().and_then(|buffer| {
            incoming
                .data
                .iter()
                .all(|value| {
                    buffer
                        .dots_for(value)
                        .all(|held_dot| held_dot.version.is_authored_by(writer))
                })
                .then_some(())
                .ok_or(Error::DataEntryWriteUnauthorized)
        })
    }

    /// Verify that `witness` retracts no element of this live carrier authored by a peer other
    /// than `writer`.
    fn permits_retraction_by(&self, witness: &RetractionWitness<'_>, writer: Did) -> Result<()> {
        self.topic_buffer().and_then(|buffer| {
            buffer
                .values
                .iter()
                .all(|(value, dot)| {
                    !witness.matches(value, dot)
                        || buffer
                            .dots_for(value)
                            .all(|held_dot| held_dot.version.is_authored_by(writer))
                })
                .then_some(())
                .ok_or(Error::DataEntryWriteUnauthorized)
        })
    }

    /// Validate a synced data carrier `self` against the live carrier `local` currently held at
    /// the storage slot (#1179).
    ///
    /// Rejects:
    /// - A synced reset floor `register` with versions below `floor`, or whose versions at or
    ///   below `floor` span another writer, or that would overwrite a live carrier holding
    ///   versions at or below `floor` from another writer (`actor != floor.actor`).
    /// - A synced remove covering a live element in `local` authored by a different writer
    ///   (`held_dot.version.actor != remove_dot.version.actor`).
    pub(crate) fn validate_data_sync_on_local(&self, local: Option<&Self>) -> Result<()> {
        self.permits_synced_register_on(local).and_then(|()| {
            Self::with_live_and_then(local, |live| self.permits_synced_tombstones_on(live))
        })
    }

    /// Whether every version in `self` at or below `floor` was authored by `floor.actor`.
    fn versions_through_floor_authored_by(&self, floor: EntryVersion) -> bool {
        self.versions()
            .filter(|&version| version <= floor)
            .all(|version| version.is_authored_by(floor.actor))
    }

    /// Verify that any reset floor on `self` or `local` is self-consistent and compatible across
    /// the two carriers.
    fn permits_synced_register_on(&self, local: Option<&Self>) -> Result<()> {
        let valid_self = self.crdt.register.is_none_or(|floor| {
            self.versions().all(|version| version >= floor)
                && self.versions_through_floor_authored_by(floor)
                && local.is_none_or(|live| live.versions_through_floor_authored_by(floor))
        });
        let valid_local = local
            .and_then(|live| live.crdt.register)
            .is_none_or(|live_floor| self.versions_through_floor_authored_by(live_floor));
        (valid_self && valid_local)
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
    }

    /// Verify that no tombstone on `self` covers a live element in `live` authored by a different
    /// writer.
    fn permits_synced_tombstones_on(&self, live: &Self) -> Result<()> {
        self.crdt.tombstones.first().map_or(Ok(()), |_| {
            let uniform_actor = self.uniform_tombstone_actor();
            let incoming_removes = self.tombstone_dots_by_element();
            live.attributed_elements()
                .all(|(value, held_dot)| {
                    Some(held_dot.version.actor) == uniform_actor
                        || incoming_removes.get(&ElementDigest::of(value)).is_none_or(
                            |remove_dot| {
                                held_dot > *remove_dot
                                    || remove_dot.version.actor == held_dot.version.actor
                            },
                        )
                })
                .then_some(())
                .ok_or(Error::DataEntryWriteUnauthorized)
        })
    }

    /// Return `Some(actor)` when every attributed tombstone in `self` shares the same `actor`.
    fn uniform_tombstone_actor(&self) -> Option<Did> {
        self.crdt
            .tombstones
            .iter()
            .map(|tombstone| tombstone.dot.version.actor)
            .filter(|actor| !actor.is_default())
            .try_fold(None, |acc, actor| match acc {
                None => Some(Some(actor)),
                Some(first) => (first == actor).then_some(Some(first)),
            })
            .flatten()
    }

    /// Map each tombstone's element digest to its covering dot.
    fn tombstone_dots_by_element(&self) -> BTreeMap<ElementDigest, EntryDot> {
        self.crdt
            .tombstones
            .iter()
            .map(|tombstone| (tombstone.element, tombstone.dot))
            .collect()
    }

    /// Iterate over `(value, dot)` pairs in `self` (both primary and co-author dots) whose dot
    /// carries a non-default author.
    fn attributed_elements(&self) -> impl Iterator<Item = (&Bytes, EntryDot)> + '_ {
        self.data
            .iter()
            .zip(self.crdt.dots.iter().copied())
            .chain(self.crdt.co_dots.iter().filter_map(|&(index, dot)| {
                self.data.get(index as usize).map(|value| (value, dot))
            }))
            .filter(|(_, dot)| dot.is_attributed())
    }

    /// Resolve the authors of element `index` in `self`, falling back to `fallback_actor` when
    /// the element's dot is absent or unattributed.
    fn element_actors_at<'a>(
        &'a self,
        index: usize,
        fallback_actor: Did,
    ) -> impl Iterator<Item = Did> + 'a {
        let primary = self
            .crdt
            .dots
            .get(index)
            .map(|dot| dot.version.actor)
            .filter(|actor| !actor.is_default())
            .unwrap_or(fallback_actor);
        let co_actors = self
            .crdt
            .co_dots
            .iter()
            .filter(move |&&(co_index, _)| co_index as usize == index)
            .map(|(_, dot)| dot.version.actor)
            .filter(|actor| !actor.is_default());
        (!primary.is_default())
            .then_some(primary)
            .into_iter()
            .chain(co_actors)
    }

    /// Map each attributed element digest in `self` to `Some(actor)` when exclusively authored by
    /// `actor`, or `None` when co-authored by multiple distinct writers.
    fn element_actors_by_digest(
        &self,
        fallback_actor: Did,
    ) -> BTreeMap<ElementDigest, Option<Did>> {
        let mut by_digest = BTreeMap::<ElementDigest, Option<Did>>::new();
        for (index, value) in self.data.iter().enumerate() {
            let mut actors = self.element_actors_at(index, fallback_actor).peekable();
            if actors.peek().is_some() {
                let digest = ElementDigest::of(value);
                for actor in actors {
                    by_digest
                        .entry(digest)
                        .and_modify(|current| {
                            if *current != Some(actor) {
                                *current = None;
                            }
                        })
                        .or_insert(Some(actor));
                }
            }
        }
        by_digest
    }

    /// Whether every element in `self` is either unattributed or authored by `writer` under
    /// `fallback_actor`.
    fn all_elements_authored_by(&self, writer: Did, fallback_actor: Did) -> bool {
        (0..self.data.len()).all(|index| {
            self.element_actors_at(index, fallback_actor)
                .all(|actor| actor == writer)
        })
    }

    /// Return `self` with any pre-planted remove in `self.crdt.tombstones` evicted when it
    /// targets an element in `incoming` authored by a different non-default writer (#1179).
    pub(crate) fn without_cross_actor_tombstones_for(
        mut self,
        incoming: &Self,
        fallback_actor: Did,
    ) -> Self {
        let needs_filtering = !incoming.data.is_empty()
            && self
                .crdt
                .tombstones
                .iter()
                .any(|tombstone| tombstone.dot.is_attributed())
            && !self
                .uniform_tombstone_actor()
                .is_some_and(|actor| incoming.all_elements_authored_by(actor, fallback_actor));
        needs_filtering.then(|| {
            let incoming_actors = incoming.element_actors_by_digest(fallback_actor);
            self.crdt.tombstones.retain(|tombstone| {
                !tombstone.dot.is_attributed()
                    || incoming_actors
                        .get(&tombstone.element)
                        .is_none_or(|actor| *actor == Some(tombstone.dot.version.actor))
            });
        });
        self
    }
}

/// Materialize normalized data-topic elements under the fair per-writer cap `max_len`,
/// returning `(data, dots, co_dots)`.
///
/// Each `(value, actor)` attribution is ranked independently by [`retain_fair_data_elements`];
/// surviving attributions for the same `Bytes` payload are then grouped so `data` and `dots`
/// hold each unique payload once at its greatest surviving dot while `co_dots` preserves any
/// surviving co-author dots `(element_index, dot)` for future merges and retraction checks.
pub(super) fn materialize_data_elements(
    values: BTreeMap<Bytes, EntryDot>,
    mut co_values: BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
    max_len: usize,
) -> (Vec<Bytes>, Vec<EntryDot>, Vec<(u32, EntryDot)>) {
    let mut visible = Vec::with_capacity(values.len());
    for (value, primary_dot) in values {
        match co_values.remove(&value) {
            Some(actors) if !actors.is_empty() => {
                if !primary_dot.is_attributed() {
                    visible.push((value.clone(), primary_dot));
                }
                visible.extend(actors.into_values().map(|dot| (value.clone(), dot)));
            }
            _ => visible.push((value, primary_dot)),
        }
    }
    visible.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
        left_dot
            .cmp(right_dot)
            .then_with(|| left_value.cmp(right_value))
    });
    if visible.len() > max_len {
        visible = retain_fair_data_elements(visible, max_len);
    }
    let mut by_value = BTreeMap::<Bytes, Vec<EntryDot>>::new();
    for (value, dot) in visible {
        by_value.entry(value).or_default().push(dot);
    }
    let mut primary = by_value
        .into_iter()
        .filter_map(|(value, mut actor_dots)| {
            actor_dots
                .pop()
                .map(|primary_dot| (value, primary_dot, actor_dots))
        })
        .collect::<Vec<_>>();
    primary.sort_by(|(left_value, left_dot, _), (right_value, right_dot, _)| {
        left_dot
            .cmp(right_dot)
            .then_with(|| left_value.cmp(right_value))
    });
    let mut data = Vec::with_capacity(primary.len());
    let mut dots = Vec::with_capacity(primary.len());
    let mut co_dots = Vec::new();
    for (index, (value, primary_dot, extra_dots)) in primary.into_iter().enumerate() {
        if let Ok(index_u32) = u32::try_from(index) {
            co_dots.extend(extra_dots.into_iter().map(|dot| (index_u32, dot)));
        }
        data.push(value);
        dots.push(primary_dot);
    }
    (data, dots, co_dots)
}

/// Retain `max_len` elements from `visible` (already sorted ascending by `(dot, value)`),
/// bounding each authenticated writer (`!dot.version.actor.is_default()`) by water-filling
/// recency rank so a single writer flooding elements cannot evict other writers' newest
/// elements (#1179).
///
/// For each element `(v, d)`, its per-actor recency rank `r` is the count of elements in
/// `visible` with the same `d.version.actor` that are strictly newer in `(dot, value)` order
/// (`r = 0` for each actor's newest element, `r = 1` for its second-newest, and `usize::MAX`
/// for unattributed legacy elements with `d.version.actor.is_default()`). Keeping the top
/// `max_len` elements under `(Reverse(r), dot, value)` and restoring canonical `(dot, value)`
/// order is a join-semilattice homomorphism: adding elements can only tighten the rank
/// threshold `r`, and reduces to standard newest-`max_len` truncation when all elements share
/// one actor or when each actor has one element.
pub(super) fn retain_fair_data_elements(
    visible: Vec<(Bytes, EntryDot)>,
    max_len: usize,
) -> Vec<(Bytes, EntryDot)> {
    let total = visible.len();
    let (_, mut ranked) = visible.into_iter().rev().fold(
        (BTreeMap::<Did, usize>::new(), Vec::with_capacity(total)),
        |(mut seen_per_actor, mut ranked), (value, dot)| {
            let rank = dot.attributed_actor().map_or(usize::MAX, |actor| {
                let count = seen_per_actor.entry(actor).or_insert(0);
                std::mem::replace(count, count.saturating_add(1))
            });
            ranked.push((rank, value, dot));
            (seen_per_actor, ranked)
        },
    );
    ranked.sort_by(
        |(left_rank, left_value, left_dot), (right_rank, right_value, right_dot)| {
            Reverse(*left_rank)
                .cmp(&Reverse(*right_rank))
                .then_with(|| left_dot.cmp(right_dot))
                .then_with(|| left_value.cmp(right_value))
        },
    );
    let skip_count = ranked.len().saturating_sub(max_len);
    let mut kept = ranked
        .into_iter()
        .skip(skip_count)
        .map(|(_, value, dot)| (value, dot))
        .collect::<Vec<_>>();
    kept.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
        left_dot
            .cmp(right_dot)
            .then_with(|| left_value.cmp(right_value))
    });
    kept
}
