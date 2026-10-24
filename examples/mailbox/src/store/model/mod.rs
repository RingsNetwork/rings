//! The store's model: mailboxes of messages, each with a UID and flags.
//!
//! A mailbox is the IMAP object of RFC 3501 §2.3: messages ordered by ascending UID, a UID
//! validity, and the next UID to assign. The model is pure; the disk shell persists it.
//!
//! ```text
//!   Mailbox = (validity : ℕ⁺, next : ℕ⁺, messages : [Message] ascending by uid)
//!   append(m, f, d, s)   = m[messages ++ (next, f, d, s), next + 1]
//!   Invariant: ∀ msg ∈ messages. uid(msg) < next,  and uids strictly ascend
//! ```
//!
//! UIDs are never reused within one validity: `next` only grows, and expunging a message
//! leaves the gap.

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

/// A message flag: one of the system flags of RFC 3501 §2.3.2, or a keyword. `\Recent` is not
/// stored: it is a property of a session, not of a message.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Flag {
    /// `\Seen`: the message has been read.
    Seen,
    /// `\Answered`: the message has been answered.
    Answered,
    /// `\Flagged`: the message is flagged for attention.
    Flagged,
    /// `\Deleted`: the message is marked for removal by a later expunge.
    Deleted,
    /// `\Draft`: the message is a draft.
    Draft,
    /// A keyword the client set.
    Keyword(String),
}

/// How a STORE changes a message's flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlagChange {
    /// Add the given flags.
    Add,
    /// Remove the given flags.
    Remove,
    /// Replace the flags with the given ones.
    Replace,
}

impl FlagChange {
    /// `flags` after this change by `given`.
    pub fn apply(self, flags: &BTreeSet<Flag>, given: &BTreeSet<Flag>) -> BTreeSet<Flag> {
        match self {
            Self::Add => flags.union(given).cloned().collect(),
            Self::Remove => flags.difference(given).cloned().collect(),
            Self::Replace => given.clone(),
        }
    }
}

/// One message's metadata; its bytes are stored apart, by UID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Message {
    /// The message's UID.
    pub uid: NonZeroU32,
    /// Its flags.
    pub flags: BTreeSet<Flag>,
    /// When the store received it, in seconds since the Unix epoch (IMAP's INTERNALDATE).
    pub internal_date: i64,
    /// Its size in bytes (IMAP's RFC822.SIZE).
    pub size: u32,
}

/// The mailbox ran out of UIDs: `2^32 − 1` messages were appended under one validity.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the mailbox has assigned every UID")]
pub struct UidsExhausted;

/// One mailbox: its UID validity, next UID, and messages.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mailbox {
    /// The UID validity: UIDs from another validity name other messages.
    pub uid_validity: NonZeroU32,
    /// The UID the next appended message gets.
    pub uid_next: NonZeroU32,
    /// The messages, by ascending UID.
    pub messages: Vec<Message>,
}

impl Mailbox {
    /// An empty mailbox with validity `uid_validity`.
    pub fn new(uid_validity: NonZeroU32) -> Self {
        Self {
            uid_validity,
            uid_next: NonZeroU32::MIN,
            messages: Vec::new(),
        }
    }

    /// Append a message of `size` bytes received at `internal_date` with `flags`; return its
    /// UID.
    ///
    /// Preservation: the new UID is `uid_next`, above every UID held, and `uid_next` grows.
    pub fn append(
        &mut self,
        flags: BTreeSet<Flag>,
        internal_date: i64,
        size: u32,
    ) -> Result<NonZeroU32, UidsExhausted> {
        let uid = self.uid_next;
        let next = uid.checked_add(1).ok_or(UidsExhausted)?;
        self.messages.push(Message {
            uid,
            flags,
            internal_date,
            size,
        });
        self.uid_next = next;
        Ok(uid)
    }

    /// The message with UID `uid`, if held.
    pub fn message(&self, uid: NonZeroU32) -> Option<&Message> {
        self.messages
            .binary_search_by_key(&uid, |message| message.uid)
            .ok()
            .and_then(|index| self.messages.get(index))
    }

    /// Change the flags of the messages whose UIDs are in `uids` by `change` with `given`;
    /// return each changed message's UID with its new flags.
    pub fn change_flags(
        &mut self,
        uids: &BTreeSet<NonZeroU32>,
        change: FlagChange,
        given: &BTreeSet<Flag>,
    ) -> Vec<(NonZeroU32, BTreeSet<Flag>)> {
        self.messages
            .iter_mut()
            .filter(|message| uids.contains(&message.uid))
            .filter_map(|message| {
                let flags = change.apply(&message.flags, given);
                (flags != message.flags).then(|| {
                    message.flags = flags.clone();
                    (message.uid, flags)
                })
            })
            .collect()
    }

    /// Remove the messages flagged `\Deleted`, only those in `only` when given; return the
    /// removed UIDs, ascending.
    pub fn expunge(&mut self, only: Option<&BTreeSet<NonZeroU32>>) -> Vec<NonZeroU32> {
        let removable = |message: &Message| {
            message.flags.contains(&Flag::Deleted)
                && only.is_none_or(|only| only.contains(&message.uid))
        };
        let removed = self
            .messages
            .iter()
            .filter(|message| removable(message))
            .map(|message| message.uid)
            .collect();
        self.messages.retain(|message| !removable(message));
        removed
    }

    /// How many messages are not `\Seen`.
    pub fn unseen(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| !message.flags.contains(&Flag::Seen))
            .count()
    }
}

/// A mailbox's special use (RFC 6154): what clients put there.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum SpecialUse {
    /// `\Sent`: copies of sent mail.
    Sent,
    /// `\Drafts`: drafts.
    Drafts,
    /// `\Trash`: deleted mail.
    Trash,
    /// `\Junk`: spam.
    Junk,
    /// `\Archive`: archived mail.
    Archive,
}

#[cfg(test)]
mod test_model;
