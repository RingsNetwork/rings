//! A session's view of its selected mailbox: the message sequence numbers its client knows,
//! and how the view catches up with the store.
//!
//! IMAP numbers messages twice: by UID, fixed, and by sequence number, the position in the
//! client's view, which changes only when the server tells the client (RFC 3501 §2.3.1.2). The
//! view is that agreement; [`View::sync`] turns the store's changes into the untagged responses
//! that move it:
//!
//! ```text
//!   expunged uids  ↦  * n EXPUNGE   highest n first, so each n is valid when sent
//!   changed flags  ↦  * n FETCH (UID u FLAGS (…))
//!   new uids       ↦  * m EXISTS    m = the new message count
//! ```
//!
//! EXPUNGE may not be sent while a command that names messages by sequence number runs (FETCH,
//! STORE, SEARCH: RFC 3501 §7.4.1), so `sync` withholds removals unless allowed; the view then
//! keeps the removed messages until a later sync may report them.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::num::NonZeroU32;

use imap_next::imap_types::sequence::SeqOrUid;
use imap_next::imap_types::sequence::Sequence;
use imap_next::imap_types::sequence::SequenceSet;

use crate::store::Flag;
use crate::store::Mailbox;
use crate::store::MailboxId;

/// What a sync has the session tell its client.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Changes {
    /// Sequence numbers to report expunged, in the order to send them.
    pub expunged: Vec<NonZeroU32>,
    /// Messages whose flags changed: sequence number, UID and new flags.
    pub flags: Vec<(NonZeroU32, NonZeroU32, BTreeSet<Flag>)>,
    /// The new message count, when it grew.
    pub exists: Option<u32>,
}

/// The selected mailbox as the client knows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct View {
    /// The mailbox.
    pub id: MailboxId,
    /// Whether it was selected read-only (EXAMINE).
    pub read_only: bool,
    /// Its UID validity.
    pub uid_validity: NonZeroU32,
    /// The UIDs the client knows, by sequence number (index + 1).
    uids: Vec<NonZeroU32>,
    /// Their flags, as last told.
    flags: BTreeMap<NonZeroU32, BTreeSet<Flag>>,
}

impl View {
    /// The view of `mailbox` (id `id`) as SELECT or EXAMINE opens it.
    pub fn open(id: MailboxId, mailbox: &Mailbox, read_only: bool) -> Self {
        Self {
            id,
            read_only,
            uid_validity: mailbox.uid_validity,
            uids: mailbox.messages.iter().map(|message| message.uid).collect(),
            flags: mailbox
                .messages
                .iter()
                .map(|message| (message.uid, message.flags.clone()))
                .collect(),
        }
    }

    /// How many messages the client knows.
    pub fn count(&self) -> u32 {
        u32::try_from(self.uids.len()).unwrap_or(u32::MAX)
    }

    /// The UIDs, by sequence number.
    pub fn uids(&self) -> &[NonZeroU32] {
        &self.uids
    }

    /// The sequence number of `uid`, if the client knows it.
    pub fn seq_of(&self, uid: NonZeroU32) -> Option<NonZeroU32> {
        let index = self.uids.binary_search(&uid).ok()?;
        u32::try_from(index.saturating_add(1))
            .ok()
            .and_then(NonZeroU32::new)
    }

    /// The flags the client was last told for `uid`.
    pub fn flags_of(&self, uid: NonZeroU32) -> Option<&BTreeSet<Flag>> {
        self.flags.get(&uid)
    }

    /// Record that the client was told `uid`'s flags are `flags`.
    pub fn told_flags(&mut self, uid: NonZeroU32, flags: BTreeSet<Flag>) {
        if self.flags.contains_key(&uid) {
            self.flags.insert(uid, flags);
        }
    }

    /// The (sequence number, UID) pairs `set` names: by sequence number, or by UID when `uid`.
    /// `*` is the last message; a UID range matches the UIDs held within it (RFC 3501 §6.4.8).
    pub fn resolve(&self, set: &SequenceSet, uid: bool) -> Vec<(NonZeroU32, NonZeroU32)> {
        let pairs = self
            .uids
            .iter()
            .enumerate()
            .filter_map(|(index, message_uid)| {
                let seq = u32::try_from(index.saturating_add(1))
                    .ok()
                    .and_then(NonZeroU32::new)?;
                Some((seq, *message_uid))
            });
        let largest = if uid {
            self.uids.last().copied()
        } else {
            NonZeroU32::new(self.count())
        };
        let Some(largest) = largest else {
            return Vec::new();
        };
        pairs
            .filter(|(seq, message_uid)| {
                let n = if uid { *message_uid } else { *seq };
                set_contains(set, n, largest)
            })
            .collect()
    }

    /// Catch up with `mailbox`, the store's current state of this view's mailbox; report
    /// removals only when `expunge_allowed`.
    ///
    /// Post: with removals allowed, the view holds exactly `mailbox`'s UIDs and flags.
    pub fn sync(&mut self, mailbox: &Mailbox, expunge_allowed: bool) -> Changes {
        let current = mailbox
            .messages
            .iter()
            .map(|message| (message.uid, &message.flags))
            .collect::<BTreeMap<_, _>>();
        let mut changes = Changes::default();
        if expunge_allowed {
            for index in (0..self.uids.len()).rev() {
                let Some(uid) = self.uids.get(index).copied() else {
                    continue;
                };
                if !current.contains_key(&uid) {
                    self.uids.remove(index);
                    self.flags.remove(&uid);
                    if let Some(seq) = u32::try_from(index.saturating_add(1))
                        .ok()
                        .and_then(NonZeroU32::new)
                    {
                        changes.expunged.push(seq);
                    }
                }
            }
        }
        for (index, uid) in self.uids.iter().enumerate() {
            let Some(flags) = current.get(uid) else {
                continue;
            };
            if self.flags.get(uid) != Some(*flags) {
                self.flags.insert(*uid, (*flags).clone());
                if let Some(seq) = u32::try_from(index.saturating_add(1))
                    .ok()
                    .and_then(NonZeroU32::new)
                {
                    changes.flags.push((seq, *uid, (*flags).clone()));
                }
            }
        }
        let last = self.uids.last().copied();
        let fresh = mailbox
            .messages
            .iter()
            .filter(|message| last.is_none_or(|last| message.uid > last))
            .collect::<Vec<_>>();
        if !fresh.is_empty() {
            for message in fresh {
                self.uids.push(message.uid);
                self.flags.insert(message.uid, message.flags.clone());
            }
            changes.exists = Some(self.count());
        }
        changes
    }
}

/// Whether `set` contains `n`, `*` standing for `largest`.
pub fn set_contains(set: &SequenceSet, n: NonZeroU32, largest: NonZeroU32) -> bool {
    let value = |bound: &SeqOrUid| match bound {
        SeqOrUid::Value(value) => *value,
        SeqOrUid::Asterisk => largest,
    };
    set.0.as_ref().iter().any(|sequence| match sequence {
        Sequence::Single(single) => value(single) == n,
        Sequence::Range(a, b) => {
            let (low, high) = (value(a).min(value(b)), value(a).max(value(b)));
            low <= n && n <= high
        }
    })
}

#[cfg(test)]
mod test_view;
