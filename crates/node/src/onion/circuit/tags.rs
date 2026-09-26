//! The client's tag table `T : t_⋄ ↦ (k_{c_n}, x, session)` (#834 D6′).
//!
//! The client is position `H + 1` of each of its loops: the guard hands it a cell whose `γ` is
//! the tag `t_⋄` the client drew for that loop. The table maps the tag to the key the returning
//! value opens under, the loop's expiry, and the session that awaits the value:
//!
//! ```text
//! register(t, k, x, s)   T ← purge(T) ∪ {t ↦ (k, x, s)}
//! deliver(t, cell, now)  (k, x, s) ← T[t], removed;   x > now ?   v ← open_k(cell);   s ! dec(v)
//! purge(T, now)          T ← { t ↦ e | x_e > now }
//! ```
//!
//! Laws (tested in `circuit::tests::test_tags`):
//!
//! - **Single use.** An entry is removed on its first delivery, before its value is opened, so a
//!   replayed reply finds no entry and is dropped.
//! - **Expiry.** An entry whose `x` has passed delivers nothing, and after `purge(now)` the table
//!   holds no entry with `x ≤ now`: `T` is empty once every `x` has passed.
//! - **Unknown tags** are dropped: [`OnionClientTags::contains`] is how the hop step tells the
//!   client's cells from a relay's.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::mpsc;

use super::OnionExpiry;
use crate::error::Result;
use crate::onion::session::frame::OnionFrame;
use crate::onion::sphinx::builder::OnionReplyKey;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::header::OnionLoopTag;
use crate::onion::sphinx::seed::OnionCarryKey;
use crate::sync_lock::lock;

/// One reply as the client's session receives it: the decoded frame and when it arrived.
#[derive(Debug)]
pub(crate) struct OnionReply {
    /// The frame `h` sent.
    pub(crate) frame: OnionFrame,
    /// The arrival instant, for the reorder window's timeout.
    pub(crate) received_at_ms: u128,
}

/// Where a session receives its replies.
pub(crate) type OnionReplySink = mpsc::Sender<OnionReply>;

/// Why a returning cell was not delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OnionReplyDropped {
    /// No live entry has the tag: unknown, already used, or expired.
    #[error("no live loop has this tag")]
    UnknownTag,
    /// The value did not open under the entry's key, or is not a frame.
    #[error("the returning value is inauthentic or malformed")]
    Inauthentic,
    /// The session is gone or its queue is full.
    #[error("the session no longer takes replies")]
    SessionGone,
}

/// One entry of `T`.
struct OnionTagEntry {
    /// `k_{c_n}`.
    key: OnionCarryKey,
    /// `x` of the loop.
    expiry: OnionExpiry,
    /// The session awaiting the reply.
    sink: OnionReplySink,
}

/// The tag table of one node, shared by the circuit shell (which delivers) and the client's
/// sessions (which register).
#[derive(Clone, Default)]
pub(crate) struct OnionClientTags {
    entries: Arc<Mutex<HashMap<OnionLoopTag, OnionTagEntry>>>,
}

impl OnionClientTags {
    /// Register the reply key of one loop for `sink` at `now`.
    pub(crate) fn register(
        &self,
        now_ms: u128,
        reply: OnionReplyKey,
        sink: OnionReplySink,
    ) -> Result<()> {
        let mut entries = lock(&self.entries)?;
        Self::purge_expired(&mut entries, now_ms);
        entries.insert(reply.tag, OnionTagEntry {
            key: reply.key,
            expiry: reply.expiry,
            sink,
        });
        Ok(())
    }

    /// Whether `tag` names an entry: the hop step's test for a cell at position `H + 1`.
    pub(crate) fn contains(&self, tag: &OnionLoopTag) -> bool {
        lock(&self.entries).is_ok_and(|entries| entries.contains_key(tag))
    }

    /// Deliver the returning `cell` of `tag` at `now` to its session, spending the entry.
    ///
    /// # Errors
    ///
    /// The [`OnionReplyDropped`] reason; the entry is spent whatever happens after it is found.
    pub(crate) fn deliver(
        &self,
        now_ms: u128,
        tag: &OnionLoopTag,
        cell: OnionCell,
    ) -> std::result::Result<(), OnionReplyDropped> {
        let entry = lock(&self.entries)
            .ok()
            .and_then(|mut entries| entries.remove(tag))
            .filter(|entry| !entry.expiry.has_passed_at(now_ms))
            .ok_or(OnionReplyDropped::UnknownTag)?;
        let class = cell.class();
        let value = cell
            .open(&entry.key)
            .map_err(|_| OnionReplyDropped::Inauthentic)?;
        let frame = OnionFrame::decode(class, value.as_slice())
            .map_err(|_| OnionReplyDropped::Inauthentic)?;
        let mut sink = entry.sink;
        sink.try_send(OnionReply {
            frame,
            received_at_ms: now_ms,
        })
        .map_err(|_| OnionReplyDropped::SessionGone)
    }

    /// Drop every entry whose `x` has passed at `now`.
    pub(crate) fn purge(&self, now_ms: u128) {
        if let Ok(mut entries) = lock(&self.entries) {
            Self::purge_expired(&mut entries, now_ms);
        }
    }

    /// The number of live entries, for tests of the expiry law.
    #[cfg(all(test, rings_native))]
    pub(crate) fn len(&self) -> usize {
        lock(&self.entries).map_or(0, |entries| entries.len())
    }

    /// `T ← { t ↦ e | x_e > now }`.
    fn purge_expired(entries: &mut HashMap<OnionLoopTag, OnionTagEntry>, now_ms: u128) {
        entries.retain(|_, entry| !entry.expiry.has_passed_at(now_ms));
    }
}
