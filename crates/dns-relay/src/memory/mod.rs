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
//! Only `Present` entries outlive a restart: the caller persists the [`MemoryChange`]s each
//! observation returns. A sighting is persisted again once it moved more than an eighth of the
//! grace period, so a restarted relay grants at least seven eighths of it. A full memory first
//! forgets what is past its window, then the entry checked longest ago, absent ones first.

use std::collections::BTreeMap;

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
    /// The owner announced `announcement`, last seen at `seen_at_ms`; `persisted_at_ms` is the
    /// sighting its persisted form holds.
    Present {
        /// The last non-empty announcement.
        announcement: Announcement,
        /// When the directory last returned it, in milliseconds since the Unix epoch.
        seen_at_ms: u64,
        /// The sighting time persisted for it.
        persisted_at_ms: u64,
    },
    /// The owner announces nothing.
    Absent,
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

/// The memory `μ` in the module doc.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnouncementMemory<O> {
    entries: BTreeMap<O, Entry>,
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
        for (owner, announcement, seen_at_ms) in remembered.into_iter().take(capacity) {
            memory.entries.insert(owner, Entry {
                known: Known::Present {
                    announcement,
                    seen_at_ms,
                    persisted_at_ms: seen_at_ms,
                },
                checked_at_ms: seen_at_ms,
            });
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
                ..
            }) if self.is_graced(*seen_at_ms, now_ms) => Some(announcement.clone()),
            _ => None,
        }
    }

    /// Fold the directory's answer `observed` for `owner` at `now_ms` into the memory (`observe`
    /// in the module doc).
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
                        changes: Vec::new(),
                    };
                }
                match unseen {
                    Ok(_none) => self.absent(owner, now_ms),
                    Err(error) => Observation {
                        answer: Err(error),
                        changes: self.forget_ungraced(&owner),
                    },
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
        let refresh = self.refresh_ms();
        let persisted = match self.entries.get(&owner).map(|entry| &entry.known) {
            Some(Known::Present {
                announcement: known,
                persisted_at_ms,
                ..
            }) if *known == announcement && now_ms.saturating_sub(*persisted_at_ms) <= refresh => {
                Some(*persisted_at_ms)
            }
            _ => None,
        };
        let mut changes = self.make_room_for(&owner, now_ms);
        if persisted.is_none() {
            changes.push(MemoryChange::Keep {
                owner,
                announcement: announcement.clone(),
                seen_at_ms: now_ms,
            });
        }
        self.entries.insert(owner, Entry {
            known: Known::Present {
                announcement: announcement.clone(),
                seen_at_ms: now_ms,
                persisted_at_ms: persisted.unwrap_or(now_ms),
            },
            checked_at_ms: now_ms,
        });
        Observation {
            answer: Ok(Some(announcement)),
            changes,
        }
    }

    /// Record that `owner` announces nothing, checked at `now_ms`.
    fn absent<E>(&mut self, owner: O, now_ms: u64) -> Observation<O, E> {
        let mut changes = match self.entries.get(&owner).map(|entry| &entry.known) {
            Some(Known::Present { .. }) => vec![MemoryChange::Forget(owner)],
            _ => Vec::new(),
        };
        changes.extend(self.make_room_for(&owner, now_ms));
        self.entries.insert(owner, Entry {
            known: Known::Absent,
            checked_at_ms: now_ms,
        });
        Observation {
            answer: Ok(None),
            changes,
        }
    }

    /// Drop `owner`'s `Present` entry when it is past the grace period: it answers nothing any
    /// more. An `Absent` entry stays, unchecked, so the next use asks the directory again.
    ///
    /// Pre: `graced(owner, now)` is `None`, so a `Present` entry here is past its grace.
    fn forget_ungraced(&mut self, owner: &O) -> Vec<MemoryChange<O>> {
        match self.entries.get(owner).map(|entry| &entry.known) {
            Some(Known::Present { .. }) => {
                self.entries.remove(owner);
                vec![MemoryChange::Forget(*owner)]
            }
            _ => Vec::new(),
        }
    }

    /// Make room for `owner` at `now_ms`, if it is not held yet: forget what is past its window,
    /// then, while the memory is full, the entry checked longest ago, absent entries first.
    ///
    /// Post: `owner ∈ dom μ ∨ |μ| < capacity`, unless the capacity is 0.
    fn make_room_for(&mut self, owner: &O, now_ms: u64) -> Vec<MemoryChange<O>> {
        if self.entries.contains_key(owner) || self.entries.len() < self.capacity {
            return Vec::new();
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
        let mut changes = expired
            .into_iter()
            .filter_map(|owner| self.evict(&owner))
            .collect::<Vec<_>>();
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
            changes.extend(self.evict(&victim));
        }
        changes
    }

    /// Remove `owner`'s entry; the change to persist when it was a `Present` one.
    fn evict(&mut self, owner: &O) -> Option<MemoryChange<O>> {
        match self.entries.remove(owner)?.known {
            Known::Present { .. } => Some(MemoryChange::Forget(*owner)),
            Known::Absent => None,
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
                    ..
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
