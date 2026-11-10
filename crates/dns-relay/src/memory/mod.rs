//! The relay's memory of what owners announce: a short freshness window that spares the
//! directory, and a long grace period that keeps a vanished owner named.
//!
//! Every DNS answer and every connection reads an owner's announcement, so the memory answers
//! from its last check while that check is fresh, a negative one included, and asks the directory
//! again only after the freshness window `f`.
//!
//! An owner's descriptor lives in the DHT only while the owner keeps writing it, so an owner
//! that goes offline vanishes from the directory within minutes. If the relay then answered
//! `NXDOMAIN`, a mail server would bounce mail to the owner permanently. The memory therefore
//! keeps the last announcement it saw for each owner and answers with it for the grace period
//! `g`; a connection meanwhile fails at the owner's node, which a sender treats as a temporary
//! failure and retries. An owner withdraws for good by announcing nothing (no port, no record),
//! which the memory takes at once.
//!
//! ```text
//!   μ : Owner ⇀ (known, checked)        known = Present(a, seen) | Absent        |μ| ≤ capacity
//!   recall(μ, o, t) = Fresh(answer(μ o))     if t − checked(μ o) < f
//!   observe(μ, o, r, t):
//!     r = Ok(a), a ≠ ∅                               ↦ μ[o ↦ (Present(a, t), t)],  answer a
//!     r = Ok(∅)                                      ↦ μ[o ↦ (Absent, t)],         answer ⊥
//!     r ∈ {Ok(⊥), Err}, μ o = Present(a, s), t−s ≤ g ↦ μ[o ↦ (Present(a, s), t)],  answer a
//!     r = Ok(⊥)                                      ↦ μ[o ↦ (Absent, t)],         answer ⊥
//!     r = Err e                                      ↦ μ without a stale Present,  answer Err e
//! ```
//!
//! Only `Present` entries outlive a restart: each observation returns the [`MemoryChange`]s owed
//! to the persisted form `π`, and [`AnnouncementMemory::mark_persisted`] (or
//! [`AnnouncementMemory::mark_compacted`]) records a change only after its journal write succeeds.
//! Unpersisted changes, removals included, stay dirty and are re-emitted until persisted. A
//! sighting is persisted again once it moved more than an eighth of the grace period, so a
//! restarted relay grants at least seven eighths of it. A full memory first forgets what is past
//! its window, then the entry checked longest ago, absent ones first.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::announcement::Announcement;
use crate::owner::Owner;

/// The windows of the memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryWindows {
    /// How long a check answers without asking the directory again, in milliseconds.
    pub fresh_ms: u64,
    /// How long a vanished owner keeps its last announcement, in milliseconds.
    pub grace_ms: u64,
}

/// What the memory knows of an owner.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Known {
    /// The owner announced `announcement`, last seen at `seen_at_ms`.
    Present {
        /// The last non-empty announcement.
        announcement: Announcement,
        /// When the directory last returned it, in milliseconds since the Unix epoch.
        seen_at_ms: u64,
    },
    /// The owner announces nothing.
    Absent,
}

/// What the persisted journal is known to hold for one owner (`π`).
#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistedEntry {
    /// The persisted non-empty announcement.
    announcement: Announcement,
    /// The sighting time persisted for it, in milliseconds since the Unix epoch.
    seen_at_ms: u64,
}

/// One owner's entry: what is known, and when the directory was last asked.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    /// What is known.
    known: Known,
    /// When the directory was last asked, in milliseconds since the Unix epoch.
    checked_at_ms: u64,
}

impl Entry {
    /// The answer this entry gives.
    fn answer(&self) -> Option<Announcement> {
        match &self.known {
            Known::Present { announcement, .. } => Some(announcement.clone()),
            Known::Absent => None,
        }
    }
}

/// What the memory recalls of an owner without asking the directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Recalled {
    /// A check within the freshness window answers: the owner's announcement, or `None`.
    Fresh(Option<Announcement>),
    /// The directory must be asked.
    Stale,
}

/// A change of the memory that its persisted form must follow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryChange<O> {
    /// `owner` announced `announcement`, last seen at `seen_at_ms`.
    Keep {
        /// The owner.
        owner: O,
        /// Its announcement.
        announcement: Announcement,
        /// When it was last seen, in milliseconds since the Unix epoch.
        seen_at_ms: u64,
    },
    /// Nothing is remembered of `owner` any more.
    Forget(O),
}

/// What one observation answers, and the changes to persist.
#[derive(Debug, Eq, PartialEq)]
pub struct Observation<O, E> {
    /// The announcement to answer with: current, or remembered within the grace period.
    pub answer: Result<Option<Announcement>, E>,
    /// The changes to persist.
    pub changes: Vec<MemoryChange<O>>,
}

/// The memory `μ` in the module doc, with its persisted shadow `π` and dirty owner set.
///
/// Invariant (`dirty`): `o ∈ dirty` iff `μ(o)` is `Present(a, s)` without a matching entry in
/// `π` within the refresh window, or `μ(o)` is not `Present` while `o ∈ dom π`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnouncementMemory<O> {
    entries: BTreeMap<O, Entry>,
    persisted: BTreeMap<O, PersistedEntry>,
    dirty: BTreeSet<O>,
    windows: MemoryWindows,
    capacity: usize,
}

impl<O> AnnouncementMemory<O>
where O: Owner
{
    /// An empty memory with `windows`, holding at most `capacity` owners.
    pub fn new(windows: MemoryWindows, capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            persisted: BTreeMap::new(),
            dirty: BTreeSet::new(),
            windows,
            capacity,
        }
    }

    /// The memory of the persisted `remembered`, as `(owner, announcement, seen_at_ms)`; empty
    /// announcements are dropped, and beyond the capacity the least recently seen owners. A
    /// restored entry was checked when it was seen, so the next use asks the directory again.
    pub fn restore(
        windows: MemoryWindows,
        capacity: usize,
        remembered: impl IntoIterator<Item = (O, Announcement, u64)>,
    ) -> Self {
        let mut memory = Self::new(windows, capacity);
        let mut remembered = remembered
            .into_iter()
            .filter(|(_owner, announcement, _seen_at_ms)| !announcement.is_empty())
            .collect::<Vec<_>>();
        remembered.sort_unstable_by_key(|(_owner, _announcement, seen_at_ms)| {
            std::cmp::Reverse(*seen_at_ms)
        });
        for (owner, announcement, seen_at_ms) in remembered {
            if memory.persisted.contains_key(&owner) {
                continue;
            }
            memory.persisted.insert(owner, PersistedEntry {
                announcement: announcement.clone(),
                seen_at_ms,
            });
            if memory.entries.len() < capacity {
                memory.entries.insert(owner, Entry {
                    known: Known::Present {
                        announcement,
                        seen_at_ms,
                    },
                    checked_at_ms: seen_at_ms,
                });
            }
            memory.sync_dirty(owner);
        }
        memory
    }

    /// How far a sighting may move before it is persisted again: an eighth of the grace period.
    const fn refresh_ms(&self) -> u64 {
        self.windows.grace_ms / 8
    }

    /// Whether a check at `checked_at_ms` is fresh at `now_ms`.
    fn is_fresh(&self, checked_at_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(checked_at_ms) < self.windows.fresh_ms
    }

    /// Whether a sighting at `seen_at_ms` is within the grace period at `now_ms`.
    fn is_graced(&self, seen_at_ms: u64, now_ms: u64) -> bool {
        now_ms.saturating_sub(seen_at_ms) <= self.windows.grace_ms
    }

    /// Maintain the dirty-set invariant for `owner`.
    fn sync_dirty(&mut self, owner: O) {
        let refresh = self.refresh_ms();
        let needs_sync = match self.entries.get(&owner).map(|entry| &entry.known) {
            Some(Known::Present {
                announcement,
                seen_at_ms,
            }) => !matches!(
                self.persisted.get(&owner),
                Some(persisted)
                    if persisted.announcement == *announcement
                        && seen_at_ms.saturating_sub(persisted.seen_at_ms) <= refresh
            ),
            Some(Known::Absent) | None => self.persisted.contains_key(&owner),
        };
        if needs_sync {
            self.dirty.insert(owner);
        } else {
            self.dirty.remove(&owner);
        }
    }

    /// Whether any [`MemoryChange`] remains unpersisted.
    pub fn has_pending_changes(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// The changes owed to the persisted form, removals before keeps.
    pub fn pending_changes(&self) -> Vec<MemoryChange<O>> {
        if self.dirty.is_empty() {
            return Vec::new();
        }
        let mut changes = Vec::with_capacity(self.dirty.len());
        for owner in &self.dirty {
            let is_present = matches!(
                self.entries.get(owner),
                Some(Entry {
                    known: Known::Present { .. },
                    ..
                })
            );
            if !is_present && self.persisted.contains_key(owner) {
                changes.push(MemoryChange::Forget(*owner));
            }
        }
        for owner in &self.dirty {
            if let Some(Entry {
                known:
                    Known::Present {
                        announcement,
                        seen_at_ms,
                    },
                ..
            }) = self.entries.get(owner)
            {
                changes.push(MemoryChange::Keep {
                    owner: *owner,
                    announcement: announcement.clone(),
                    seen_at_ms: *seen_at_ms,
                });
            }
        }
        changes
    }

    /// Record that `change` has been durably written to the journal.
    pub fn mark_persisted(&mut self, change: &MemoryChange<O>) {
        match change {
            MemoryChange::Keep {
                owner,
                announcement,
                seen_at_ms,
            } => {
                self.persisted.insert(*owner, PersistedEntry {
                    announcement: announcement.clone(),
                    seen_at_ms: *seen_at_ms,
                });
                self.sync_dirty(*owner);
            }
            MemoryChange::Forget(owner) => {
                self.persisted.remove(owner);
                self.sync_dirty(*owner);
            }
        }
    }

    /// Record that the journal has been atomically compacted to [`Self::remembered`].
    pub fn mark_compacted(&mut self) {
        self.persisted = self
            .entries
            .iter()
            .filter_map(|(owner, entry)| match &entry.known {
                Known::Present {
                    announcement,
                    seen_at_ms,
                } => Some((*owner, PersistedEntry {
                    announcement: announcement.clone(),
                    seen_at_ms: *seen_at_ms,
                })),
                Known::Absent => None,
            })
            .collect();
        self.dirty.clear();
    }

    /// What the memory recalls of `owner` at `now_ms` (`recall` in the module doc).
    pub fn recall(&self, owner: &O, now_ms: u64) -> Recalled {
        match self.entries.get(owner) {
            Some(entry) if self.is_fresh(entry.checked_at_ms, now_ms) => {
                Recalled::Fresh(entry.answer())
            }
            _ => Recalled::Stale,
        }
    }

    /// The announcement `owner` keeps within the grace period at `now_ms`, if any: what the relay
    /// may answer when it cannot ask the directory.
    pub fn graced(&self, owner: &O, now_ms: u64) -> Option<Announcement> {
        match self.entries.get(owner).map(|entry| &entry.known) {
            Some(Known::Present {
                announcement,
                seen_at_ms,
            }) if self.is_graced(*seen_at_ms, now_ms) => Some(announcement.clone()),
            _ => None,
        }
    }

    /// Fold the directory's answer `observed` for `owner` at `now_ms` into the memory (`observe`
    /// in the module doc). Emitted changes remain pending until [`Self::mark_persisted`] or
    /// [`Self::mark_compacted`] confirms they reached disk.
    pub fn observe<E>(
        &mut self,
        owner: O,
        observed: Result<Option<Announcement>, E>,
        now_ms: u64,
    ) -> Observation<O, E> {
        match observed {
            Ok(Some(announcement)) if !announcement.is_empty() => {
                self.present(owner, announcement, now_ms)
            }
            Ok(Some(_withdrawn)) => self.absent(owner, now_ms),
            unseen => {
                if let Some(graced) = self.graced(&owner, now_ms) {
                    if let Some(entry) = self.entries.get_mut(&owner) {
                        entry.checked_at_ms = now_ms;
                    }
                    return Observation {
                        answer: Ok(Some(graced)),
                        changes: self.pending_changes(),
                    };
                }
                match unseen {
                    Ok(_none) => self.absent(owner, now_ms),
                    Err(error) => {
                        self.forget_ungraced(&owner);
                        Observation {
                            answer: Err(error),
                            changes: self.pending_changes(),
                        }
                    }
                }
            }
        }
    }

    /// Record that `owner` announces the non-empty `announcement`, seen at `now_ms`.
    fn present<E>(
        &mut self,
        owner: O,
        announcement: Announcement,
        now_ms: u64,
    ) -> Observation<O, E> {
        self.make_room_for(&owner, now_ms);
        self.entries.insert(owner, Entry {
            known: Known::Present {
                announcement: announcement.clone(),
                seen_at_ms: now_ms,
            },
            checked_at_ms: now_ms,
        });
        self.sync_dirty(owner);
        Observation {
            answer: Ok(Some(announcement)),
            changes: self.pending_changes(),
        }
    }

    /// Record that `owner` announces nothing, checked at `now_ms`.
    fn absent<E>(&mut self, owner: O, now_ms: u64) -> Observation<O, E> {
        self.make_room_for(&owner, now_ms);
        self.entries.insert(owner, Entry {
            known: Known::Absent,
            checked_at_ms: now_ms,
        });
        self.sync_dirty(owner);
        Observation {
            answer: Ok(None),
            changes: self.pending_changes(),
        }
    }

    /// Drop `owner`'s `Present` entry when it is past the grace period: it answers nothing any
    /// more. An `Absent` entry stays, unchecked, so the next use asks the directory again.
    ///
    /// Pre: `graced(owner, now)` is `None`, so a `Present` entry here is past its grace.
    fn forget_ungraced(&mut self, owner: &O) {
        if matches!(
            self.entries.get(owner).map(|entry| &entry.known),
            Some(Known::Present { .. })
        ) {
            self.entries.remove(owner);
        }
        self.sync_dirty(*owner);
    }

    /// Make room for `owner` at `now_ms`, if it is not held yet: forget what is past its window,
    /// then, while the memory is full, the entry checked longest ago, absent entries first.
    ///
    /// Post: `owner ∈ dom μ ∨ |μ| < capacity`, unless the capacity is 0.
    fn make_room_for(&mut self, owner: &O, now_ms: u64) {
        if self.entries.contains_key(owner) || self.entries.len() < self.capacity {
            return;
        }
        let windows = self.windows;
        let expired = self
            .entries
            .iter()
            .filter(|(_owner, entry)| match &entry.known {
                Known::Present { seen_at_ms, .. } => {
                    now_ms.saturating_sub(*seen_at_ms) > windows.grace_ms
                }
                Known::Absent => now_ms.saturating_sub(entry.checked_at_ms) >= windows.fresh_ms,
            })
            .map(|(owner, _entry)| *owner)
            .collect::<Vec<_>>();
        for expired_owner in expired {
            self.evict(&expired_owner);
        }
        while !self.entries.is_empty() && self.entries.len() >= self.capacity {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_owner, entry)| {
                    (
                        matches!(entry.known, Known::Present { .. }),
                        entry.checked_at_ms,
                    )
                })
                .map(|(owner, _entry)| *owner);
            let Some(victim) = victim else {
                break;
            };
            self.evict(&victim);
        }
    }

    /// Remove `owner`'s entry and update its dirty state when it was persisted.
    fn evict(&mut self, owner: &O) {
        if self.entries.remove(owner).is_some() {
            self.sync_dirty(*owner);
        }
    }

    /// The remembered announcements, as `(owner, announcement, seen_at_ms)`: what the persisted
    /// memory holds.
    pub fn remembered(&self) -> impl Iterator<Item = (O, &Announcement, u64)> + '_ {
        self.entries
            .iter()
            .filter_map(|(owner, entry)| match &entry.known {
                Known::Present {
                    announcement,
                    seen_at_ms,
                } => Some((*owner, announcement, *seen_at_ms)),
                Known::Absent => None,
            })
    }

    /// How many owners the memory holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the memory holds no owner.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod test_memory;
