//! The relay's memory of announcements: an owner that goes offline keeps its name for a grace
//! period.
//!
//! An owner's descriptor lives in the DHT only while the owner keeps writing it, so an owner
//! that goes offline vanishes from the directory within minutes. If the relay then answered
//! `NXDOMAIN`, a mail server would bounce mail to the owner permanently. The relay therefore
//! remembers the last announcement it saw for each owner and keeps answering with it for a grace
//! period. A connection meanwhile fails at the owner's node, which a sender treats as a
//! temporary failure and retries. An owner withdraws for good by announcing nothing (no port,
//! no record), which the relay forgets at once.
//!
//! ```text
//!   μ : Owner ⇀ (Announcement, seen)                 the memory
//!   observe(μ, o, r, t):
//!     r = Ok(a), a ≠ ∅          ↦  μ[o ↦ (a, t)],  answer a
//!     r = Ok(∅)                 ↦  μ ∖ o,          answer ⊥        withdrawn
//!     r ∈ {Ok(⊥), Err e}, μ o = (a, s), t − s ≤ g   ↦  μ, answer a  remembered
//!     r = Ok(⊥)                 ↦  μ ∖ o,          answer ⊥
//!     r = Err e                 ↦  μ ∖ o,          answer Err e
//!   Invariant: every remembered announcement is non-empty.
//! ```
//!
//! The memory is pure: the caller supplies the time and persists it. [`Observation::changed`]
//! says when it should: whenever an announcement is learned, changed or forgotten, and when a
//! sighting has moved more than [`AnnouncementMemory::refresh_ms`] since it was last persisted,
//! so the grace a restarted relay grants is short by at most that much.

use std::collections::BTreeMap;

use crate::announcement::Announcement;
use crate::capability::LookupFailure;
use crate::owner::Owner;

/// One remembered announcement: what the owner announced, when the relay last saw it, and the
/// sighting time the persisted memory holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remembered {
    /// The owner's last announcement; never empty.
    pub announcement: Announcement,
    /// When the relay last saw it, in milliseconds since the Unix epoch.
    pub seen_at_ms: u64,
    /// The sighting time the persisted memory holds for this announcement; `None` until this
    /// announcement has been persisted.
    pub persisted_seen_at_ms: Option<u64>,
}

/// What one observation answers, and whether the memory changed in a way worth persisting.
#[derive(Debug, Eq, PartialEq)]
pub struct Observation {
    /// The announcement to answer with: current, or remembered within the grace period.
    pub answer: Result<Option<Announcement>, LookupFailure>,
    /// Whether the memory should be persisted.
    pub changed: bool,
}

/// The memory `μ` in the module doc, with its grace period `g`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnouncementMemory<O> {
    entries: BTreeMap<O, Remembered>,
    grace_ms: u64,
}

impl<O> AnnouncementMemory<O>
where O: Owner
{
    /// An empty memory with grace period `grace_ms`.
    pub fn new(grace_ms: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            grace_ms,
        }
    }

    /// The memory holding `entries`, as persisted; empty announcements are dropped.
    pub fn from_entries(
        grace_ms: u64,
        entries: impl IntoIterator<Item = (O, Announcement, u64)>,
    ) -> Self {
        let entries = entries
            .into_iter()
            .filter(|(_owner, announcement, _seen)| !announcement.is_empty())
            .map(|(owner, announcement, seen_at_ms)| {
                (owner, Remembered {
                    announcement,
                    seen_at_ms,
                    persisted_seen_at_ms: Some(seen_at_ms),
                })
            })
            .collect();
        Self { entries, grace_ms }
    }

    /// How far a sighting may move before the memory asks to be persisted: an eighth of the
    /// grace period.
    pub const fn refresh_ms(&self) -> u64 {
        self.grace_ms / 8
    }

    /// Whether `remembered` is within the grace period at `now_ms`.
    fn within_grace(&self, remembered: &Remembered, now_ms: u64) -> bool {
        now_ms.saturating_sub(remembered.seen_at_ms) <= self.grace_ms
    }

    /// Observe the directory's answer `observed` for `owner` at `now_ms` (`observe` in the
    /// module doc).
    pub fn observe(
        &mut self,
        owner: O,
        observed: Result<Option<Announcement>, LookupFailure>,
        now_ms: u64,
    ) -> Observation {
        match observed {
            Ok(Some(announcement)) if announcement.is_empty() => Observation {
                answer: Ok(None),
                changed: self.entries.remove(&owner).is_some(),
            },
            Ok(Some(announcement)) => {
                let refresh = self.refresh_ms();
                let persisted_seen_at_ms = self
                    .entries
                    .get(&owner)
                    .filter(|remembered| remembered.announcement == announcement)
                    .and_then(|remembered| remembered.persisted_seen_at_ms);
                let changed = persisted_seen_at_ms
                    .is_none_or(|persisted| now_ms.saturating_sub(persisted) > refresh);
                self.entries.insert(owner, Remembered {
                    announcement: announcement.clone(),
                    seen_at_ms: now_ms,
                    persisted_seen_at_ms,
                });
                Observation {
                    answer: Ok(Some(announcement)),
                    changed,
                }
            }
            unseen => match self.entries.get(&owner) {
                Some(remembered) if self.within_grace(remembered, now_ms) => Observation {
                    answer: Ok(Some(remembered.announcement.clone())),
                    changed: false,
                },
                _ => Observation {
                    answer: unseen,
                    changed: self.entries.remove(&owner).is_some(),
                },
            },
        }
    }

    /// Forget every announcement past its grace period at `now_ms`; return whether any was.
    pub fn forget_expired(&mut self, now_ms: u64) -> bool {
        let before = self.entries.len();
        let grace_ms = self.grace_ms;
        self.entries
            .retain(|_owner, remembered| now_ms.saturating_sub(remembered.seen_at_ms) <= grace_ms);
        self.entries.len() != before
    }

    /// Record that the memory as it stands now has been persisted.
    pub fn mark_persisted(&mut self) {
        for remembered in self.entries.values_mut() {
            remembered.persisted_seen_at_ms = Some(remembered.seen_at_ms);
        }
    }

    /// The remembered announcements, by owner.
    pub fn entries(&self) -> impl Iterator<Item = (&O, &Remembered)> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod test_memory;
