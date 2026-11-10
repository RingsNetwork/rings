//! The client's tag table `T : t_⋄ ↦ (k_{c_n}, x, target)` (#834 D6′).
//!
//! The client is position `H + 1` of each of its loops: the guard hands it a cell whose `γ` is
//! the tag `t_⋄` the client drew for that loop. The table maps the tag to the key the returning
//! value opens under, the loop's expiry, and the target awaiting the value: either a streaming
//! session (`OnionFrame`s) or a one-shot Kleisli pipeline (`W(fₙ)`-byte output).
//!
//! Laws (tested in `circuit::tests::test_tags`):
//!
//! - **Single use.** An entry is removed on its first delivery, before its value is opened, so a
//!   replayed reply finds no entry and is dropped.
//! - **Expiry.** An entry whose `x` has passed delivers nothing, and after `purge(now)` the table
//!   holds no entry with `x ≤ now`: `T` is empty once every `x` has passed.
//! - **Unknown tags** are dropped: [`OnionClientTags::expects`] is how the hop step tells the
//!   client's cells from a relay's.
//! - **Guard binding.** An entry is recognised only on a cell from the guard `g` its loop returns
//!   through, the only peer that sees `t_⋄`; a cell from any other peer is a relay's, and the
//!   entry stays.
//! - **Fate** (#963, #966, #969, #971). `lose(p)` closes every session and drops every one-shot
//!   sender whose loop returns through `p`, so awaiting callers observe the loss immediately.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use futures::channel::mpsc;
use futures::channel::oneshot;
use rings_core::dht::Did;

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
    /// The value did not open under the entry's key, is not a valid frame, or does not match `W(fₙ)`.
    #[error("the returning value is inauthentic or malformed")]
    Inauthentic,
    /// The session or one-shot caller is gone or its queue is full.
    #[error("the session no longer takes replies")]
    SessionGone,
}

enum OnionReplyTarget {
    Session(OnionReplySink),
    OneShot {
        expected_bytes: usize,
        sender: oneshot::Sender<Vec<u8>>,
    },
}

/// One entry of `T`.
struct OnionTagEntry {
    /// `g`, the guard the loop returns through.
    guard: Did,
    /// `k_{c_n}`.
    key: OnionCarryKey,
    /// `x` of the loop.
    expiry: OnionExpiry,
    /// The session or one-shot caller awaiting the reply.
    target: OnionReplyTarget,
}

/// The tag table of one node, shared by the circuit shell (which delivers) and the client's
/// sessions and pipelines (which register).
#[derive(Clone, Default)]
pub(crate) struct OnionClientTags {
    /// The entries, keyed by tag.
    entries: Arc<Mutex<HashMap<OnionLoopTag, OnionTagEntry>>>,
}

impl OnionClientTags {
    /// Register at `now` the reply key of one session loop returning through `guard`, for `sink`.
    pub(crate) fn register(
        &self,
        now_ms: u128,
        guard: Did,
        reply: OnionReplyKey,
        sink: OnionReplySink,
    ) -> Result<()> {
        self.insert_entry(now_ms, guard, reply, OnionReplyTarget::Session(sink))
    }

    /// Register at `now` the reply key of one one-shot Kleisli pipeline loop returning `expected_bytes`
    /// through `guard`.
    pub(crate) fn register_oneshot(
        &self,
        now_ms: u128,
        guard: Did,
        reply: OnionReplyKey,
        expected_bytes: usize,
        sender: oneshot::Sender<Vec<u8>>,
    ) -> Result<()> {
        self.insert_entry(now_ms, guard, reply, OnionReplyTarget::OneShot {
            expected_bytes,
            sender,
        })
    }

    fn insert_entry(
        &self,
        now_ms: u128,
        guard: Did,
        reply: OnionReplyKey,
        target: OnionReplyTarget,
    ) -> Result<()> {
        let mut entries = lock(&self.entries)?;
        Self::purge_expired(&mut entries, now_ms);
        entries.insert(reply.tag, OnionTagEntry {
            guard,
            key: reply.key,
            expiry: reply.expiry,
            target,
        });
        Ok(())
    }

    /// Remove a specific tag if still unspent (e.g., on local pipeline timeout or cancellation).
    pub(crate) fn remove(&self, tag: &OnionLoopTag) {
        if let Ok(mut entries) = lock(&self.entries) {
            entries.remove(tag);
        }
    }

    /// Whether a cell from `from` with tag `tag` is one of this node's returning loops: the hop
    /// step's test for a cell at position `H + 1`.
    pub(crate) fn expects(&self, from: Did, tag: &OnionLoopTag) -> bool {
        lock(&self.entries)
            .is_ok_and(|entries| entries.get(tag).is_some_and(|entry| entry.guard == from))
    }

    /// Deliver the returning `cell` of `tag` from `from` at `now` to its session or one-shot
    /// caller, spending the entry if `from` is its guard.
    pub(crate) fn deliver(
        &self,
        now_ms: u128,
        from: Did,
        tag: &OnionLoopTag,
        cell: OnionCell,
    ) -> std::result::Result<(), OnionReplyDropped> {
        let entry = lock(&self.entries)
            .ok()
            .and_then(|mut entries| {
                let guarded = entries.get(tag).is_some_and(|entry| entry.guard == from);
                guarded.then(|| entries.remove(tag)).flatten()
            })
            .filter(|entry| !entry.expiry.has_passed_at(now_ms))
            .ok_or(OnionReplyDropped::UnknownTag)?;
        let class = cell.class();
        let value = cell
            .open(&entry.key)
            .map_err(|_| OnionReplyDropped::Inauthentic)?;
        match entry.target {
            OnionReplyTarget::Session(mut sink) => {
                let frame = OnionFrame::decode(class, value.as_slice())
                    .map_err(|_| OnionReplyDropped::Inauthentic)?;
                sink.try_send(OnionReply {
                    frame,
                    received_at_ms: now_ms,
                })
                .map_err(|refused| {
                    // A full queue loses this reply: close the session rather than leave a gap (Fate).
                    if refused.is_full() {
                        sink.close_channel();
                    }
                    OnionReplyDropped::SessionGone
                })
            }
            OnionReplyTarget::OneShot {
                expected_bytes,
                sender,
            } => {
                if value.as_slice().len() != expected_bytes {
                    return Err(OnionReplyDropped::Inauthentic);
                }
                sender
                    .send(value.as_slice().to_vec())
                    .map_err(|_| OnionReplyDropped::SessionGone)
            }
        }
    }

    /// Remove the entries of `tags` when their carrying loop or credit cell failed to leave this
    /// node.
    pub(crate) fn unregister_all(&self, tags: impl IntoIterator<Item = OnionLoopTag>) {
        if let Ok(mut entries) = lock(&self.entries) {
            for tag in tags {
                entries.remove(&tag);
            }
        }
    }

    /// `lose(T, p)`: the link to `guard` retired or dropped a message, so any reply returning
    /// through it is lost or may be. Close sessions and drop one-shot senders riding `guard`.
    pub(crate) fn lose(&self, guard: Did) {
        let Ok(mut entries) = lock(&self.entries) else {
            return;
        };
        entries.retain(|_, entry| {
            let rides = entry.guard == guard;
            if rides {
                if let OnionReplyTarget::Session(sink) = &mut entry.target {
                    sink.close_channel();
                }
            }
            !rides
        });
    }

    /// Drop every entry whose `x` has passed at `now`, as every registration does first; for
    /// tests of the expiry law.
    #[cfg(all(test, rings_native))]
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
