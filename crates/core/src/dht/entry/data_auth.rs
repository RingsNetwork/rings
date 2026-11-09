//! Data-topic write authentication and fair per-writer element retention (#1179).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bytes::Bytes;

use super::ElementDigest;
use super::Entry;
use super::EntryDot;
use super::EntryOperation;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

impl EntryOperation {
    /// Validate a data-topic write operation `self` issued by `writer` and introduced by
    /// `introducer` at `now_ms` inside `network_id` (#1179).
    ///
    /// Enforces:
    /// - Bounds and liveness (`validate_admissible_at`).
    /// - Every non-default version carried by the operation (`register`, `dots`, `tombstones`)
    ///   was authored by `writer`.
    /// - `Overwrite` carries no raw tombstones and, when delivered from a remote peer, carries
    ///   at least one replacement element.
    /// - `Extend` and `Tombstone` carry neither a reset register nor raw tombstones.
    pub(crate) fn validate_data_write(
        &self,
        now_ms: u128,
        network_id: u32,
        writer: Did,
        introducer: crate::message::Principal,
    ) -> Result<()> {
        self.validate_admissible_at(now_ms, network_id)?;
        let entry = self.entry();
        if entry
            .versions()
            .any(|version| version.actor != writer && version.actor != Did::default())
        {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        match self {
            EntryOperation::Overwrite(entry) => {
                if !entry.crdt.tombstones.is_empty() {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                if entry.data.is_empty() && introducer != crate::message::Principal::Local {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
            }
            EntryOperation::Extend(entry) | EntryOperation::Tombstone(entry) => {
                if entry.crdt.register.is_some() || !entry.crdt.tombstones.is_empty() {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
            }
        }
        Ok(())
    }

    /// Validate a data-topic operation `self` issued by `writer` against the live carrier
    /// `local` currently held at the storage slot (#1179).
    ///
    /// - `Overwrite`: rejected when `entry.data` is empty against a live carrier, or when `local`
    ///   holds any live element, remove, or reset floor authored by another writer (`actor !=
    ///   writer`).
    /// - `Tombstone`: rejected when any matched element in `local` (by dot witness or value
    ///   witness) was authored by another writer (`held_dot.version.actor != writer`).
    pub(crate) fn validate_data_write_on_local(
        &self,
        local: Option<&Entry>,
        writer: Did,
    ) -> Result<()> {
        let Some(live) = local else {
            return Ok(());
        };
        match self {
            EntryOperation::Overwrite(entry) => {
                if entry.data.is_empty()
                    || live
                        .versions()
                        .any(|version| version.actor != writer && version.actor != Did::default())
                {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
            }
            EntryOperation::Tombstone(entry) => {
                let buffer = live.topic_buffer()?;
                let has_dot_witness = entry.crdt.has_write_witness() && !entry.crdt.dots.is_empty();
                let target_dots = entry.crdt.dots.iter().copied().collect::<BTreeSet<_>>();
                let target_values = entry.data.iter().collect::<BTreeSet<_>>();
                if buffer.values.into_iter().any(|(value, held_dot)| {
                    let matched = if has_dot_witness {
                        target_dots.contains(&held_dot)
                    } else {
                        target_values.contains(&value)
                    };
                    matched
                        && held_dot.version.actor != writer
                        && held_dot.version.actor != Did::default()
                }) {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
            }
            EntryOperation::Extend(_) => {}
        }
        Ok(())
    }
}

impl Entry {
    /// Validate a synced data carrier `self` against the live carrier `local` currently held at
    /// the storage slot (#1179).
    ///
    /// Rejects:
    /// - A synced reset floor `register` with empty `data`, or whose versions span another writer,
    ///   or that would overwrite a live carrier holding versions from another writer (`actor !=
    ///   floor.actor`).
    /// - A synced remove covering a live element in `local` authored by a different writer
    ///   (`held_dot.version.actor != remove_dot.version.actor`).
    pub(crate) fn validate_data_sync_on_local(&self, local: Option<&Entry>) -> Result<()> {
        if let Some(floor) = self.crdt.register {
            if self.data.is_empty()
                || self
                    .versions()
                    .any(|version| version.actor != floor.actor && version.actor != Did::default())
                || local.is_some_and(|live| {
                    live.versions().any(|version| {
                        version.actor != floor.actor && version.actor != Did::default()
                    })
                })
            {
                return Err(Error::DataEntryWriteUnauthorized);
            }
        }
        if let (Some(live), Some(first_remove)) = (local, self.crdt.tombstones.first()) {
            let first_remove_actor = first_remove.dot.version.actor;
            let single_remove_actor = self
                .crdt
                .tombstones
                .iter()
                .all(|tombstone| tombstone.dot.version.actor == first_remove_actor)
                .then_some(first_remove_actor);
            let incoming_removes = self
                .crdt
                .tombstones
                .iter()
                .map(|tombstone| (tombstone.element, tombstone.dot))
                .collect::<BTreeMap<_, _>>();
            if live
                .data
                .iter()
                .zip(live.crdt.dots.iter())
                .any(|(value, held_dot)| {
                    held_dot.version.actor != Did::default()
                        && Some(held_dot.version.actor) != single_remove_actor
                        && incoming_removes.get(&ElementDigest::of(value)).is_some_and(
                            |remove_dot| {
                                held_dot <= remove_dot
                                    && remove_dot.version.actor != held_dot.version.actor
                            },
                        )
                })
            {
                return Err(Error::DataEntryWriteUnauthorized);
            }
        }
        Ok(())
    }

    /// Return `self` with any pre-planted remove in `self.crdt.tombstones` evicted when it
    /// targets an element in `incoming` authored by a different non-default writer (#1179).
    pub(crate) fn without_cross_actor_tombstones_for(
        mut self,
        incoming: &Entry,
        fallback_actor: Did,
    ) -> Self {
        let mut tombstone_actors = self
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| tombstone.dot.version.actor)
            .filter(|actor| *actor != Did::default());
        let Some(first_tombstone_actor) = tombstone_actors.next() else {
            return self;
        };
        if incoming.data.is_empty() {
            return self;
        }
        let single_tombstone_actor = tombstone_actors
            .all(|actor| actor == first_tombstone_actor)
            .then_some(first_tombstone_actor);
        let actor_at = |index: usize| {
            incoming
                .crdt
                .dots
                .get(index)
                .map(|dot| dot.version.actor)
                .filter(|actor| *actor != Did::default())
                .unwrap_or(fallback_actor)
        };
        if single_tombstone_actor.is_some_and(|tombstone_actor| {
            (0..incoming.data.len()).all(|index| {
                let actor = actor_at(index);
                actor == Did::default() || actor == tombstone_actor
            })
        }) {
            return self;
        }
        let incoming_actors = incoming
            .data
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let actor = actor_at(index);
                (actor != Did::default()).then(|| (ElementDigest::of(value), actor))
            })
            .collect::<BTreeMap<_, _>>();
        self.crdt.tombstones.retain(|tombstone| {
            tombstone.dot.version.actor == Did::default()
                || incoming_actors
                    .get(&tombstone.element)
                    .is_none_or(|actor| *actor == tombstone.dot.version.actor)
        });
        self
    }
}

/// Retain `max_len` elements from `visible` (already sorted ascending by `(dot, value)`),
/// bounding each authenticated writer (`dot.version.actor != Did::default()`) by water-filling
/// recency rank so a single writer flooding elements cannot evict other writers' newest
/// elements (#1179).
///
/// For each element `(v, d)`, its per-actor recency rank `r` is the count of elements in
/// `visible` with the same `d.version.actor` that are strictly newer in `(dot, value)` order
/// (`r = 0` for each actor's newest element, `r = 1` for its second-newest, and `usize::MAX`
/// for unattributed legacy elements with `d.version.actor == Did::default()`). Keeping the top
/// `max_len` elements under `(Reverse(r), dot, value)` and restoring canonical `(dot, value)`
/// order is a join-semilattice homomorphism: adding elements can only tighten the rank
/// threshold `r`, and reduces to standard newest-`max_len` truncation when all elements share
/// one actor or when each actor has one element.
pub(super) fn retain_fair_data_elements(
    visible: Vec<(Bytes, EntryDot)>,
    max_len: usize,
) -> Vec<(Bytes, EntryDot)> {
    let mut seen_per_actor = BTreeMap::<Did, usize>::new();
    let mut ranked = Vec::with_capacity(visible.len());
    for (value, dot) in visible.into_iter().rev() {
        let rank = if dot.version.actor == Did::default() {
            usize::MAX
        } else {
            let count = seen_per_actor.entry(dot.version.actor).or_insert(0);
            let current = *count;
            *count = current.saturating_add(1);
            current
        };
        ranked.push((rank, value, dot));
    }
    ranked.sort_by(
        |(left_rank, left_value, left_dot), (right_rank, right_value, right_dot)| {
            std::cmp::Reverse(*left_rank)
                .cmp(&std::cmp::Reverse(*right_rank))
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
