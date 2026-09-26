//! The client's session machine and credit window (#834 D2′, D6′, D8).
//!
//! ```text
//! data(w):   frame data(n, T?, w), n ← n + 1;  T is set, with t inline, until the first reply
//! fin:       frame fin(n), n ← n + 1
//! reply(f):  replied ← ⊤;  reorder f, then per released frame, in order:
//!   first frame   data(0, ε) ⇒ Opened      fin ⇒ Refused (a fin before any data: no reason given)
//!   later frames  data(w)    ⇒ Data(w)     fin ⇒ Fin
//! credit:    want min(W, Q_max) reply blocks outstanding at h; each forward loop leaves one and a
//!            credit loop of j ≤ k blocks leaves j + 1, so the deficit D asks ⌊D / (k + 1)⌋ full
//!            credit loops and, for a remainder r ≥ 2, one of r − 1 blocks
//! ```
//!
//! Laws (tested in `session::tests`):
//!
//! - **Target.** Every `data` frame sent before the first reply carries `T` and the target; none
//!   after it does, so the session opens at `h` whichever forward loop arrives first and the
//!   target stops travelling once `h` has answered.
//! - **Sequence.** Forward frames carry `n = 0, 1, …`; replies are released in their order.
//! - **Open result.** The first released reply decides the open: `data(0, ε)` is the ack, `fin`
//!   is a refusal (#843 Q5).
//! - **Credit.** The window never asks for more than `min(W, Q_max)` outstanding blocks, and
//!   the client's count bounds the blocks at `h` from above, with equality absent loss and up to
//!   clock skew at an expiry: `h` spends the block of least expiry first, and so does
//!   [`OnionClientCredit`]; both drop a block at its expiry; and the count saturates at `Q_max`,
//!   where `h` refuses further blocks.

use std::collections::BTreeMap;

use bytes::Bytes;

use super::exit::OnionStreamFrame;
use super::frame::OnionFrame;
use super::frame::OnionSequence;
use super::order::OnionReorder;
use super::order::OnionSequenceGap;
use super::pool::ONION_SURB_POOL_CAPACITY;
use crate::onion::circuit::OnionExpiry;
use crate::onion::sphinx::class::OnionLoopClass;

/// What the session learned from one reply, for the client shell to act on.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum OnionClientEvent {
    /// `h` opened the world: the open ack.
    Opened,
    /// `h` refused or failed to open the world; no reason is given (#843 Q5).
    Refused,
    /// Stream bytes from the world.
    Data(Bytes),
    /// The world closed its stream.
    Fin,
}

/// Why the session can send no further frame: its 32-bit sequence space is spent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("session sequence space exhausted")]
pub(crate) struct OnionSequenceExhausted;

/// The client's machine of one session; see the module documentation.
#[derive(Debug)]
pub(crate) struct OnionClientSession {
    /// `t`, the canonical target authority.
    target: Bytes,
    /// The sequence of the next forward frame; `None` once spent.
    forward: Option<OnionSequence>,
    /// Whether any reply of the session has arrived: `T` stops.
    replied: bool,
    /// Whether the open has been decided by the first released reply.
    decided: bool,
    /// The `h`-to-client direction.
    replies: OnionReorder<OnionStreamFrame>,
}

impl OnionClientSession {
    /// A session to the target with the canonical authority `target`; its arguments
    /// `ā = ς ‖ SHA-256(t)` are the shell's.
    pub(crate) fn new(target: Bytes) -> Self {
        Self {
            target,
            forward: Some(OnionSequence::FIRST),
            replied: false,
            decided: false,
            replies: OnionReorder::default(),
        }
    }

    /// The most stream bytes the next `data` frame can carry in `class`: less the target while
    /// `T` is still set.
    pub(crate) fn data_capacity(&self, class: OnionLoopClass) -> usize {
        if self.replied {
            OnionFrame::data_capacity(class)
        } else {
            OnionFrame::data_capacity_with_target(class, &self.target).unwrap_or(0)
        }
    }

    /// The next `data(n, T?, w)` frame.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceExhausted`] once the sequence space is spent.
    pub(crate) fn data(&mut self, payload: Bytes) -> Result<OnionFrame, OnionSequenceExhausted> {
        let sequence = self.advance()?;
        Ok(OnionFrame::Data {
            sequence,
            target: (!self.replied).then(|| self.target.clone()),
            payload,
        })
    }

    /// The next `fin(n)` frame.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceExhausted`] once the sequence space is spent.
    pub(crate) fn fin(&mut self) -> Result<OnionFrame, OnionSequenceExhausted> {
        self.advance().map(|sequence| OnionFrame::Fin { sequence })
    }

    /// One reply of the session at `now`, returning what it releases, in order.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceGap`] if the reply direction has a gap that can no longer fill; the
    /// session then fails closed.
    pub(crate) fn reply(
        &mut self,
        now_ms: u128,
        frame: OnionFrame,
    ) -> Result<Vec<OnionClientEvent>, OnionSequenceGap> {
        self.replied = true;
        let (sequence, frame) = match frame {
            OnionFrame::Data {
                sequence, payload, ..
            } => (sequence, OnionStreamFrame::Data {
                target: None,
                payload,
            }),
            OnionFrame::Fin { sequence } => (sequence, OnionStreamFrame::Fin),
            // `h` never sends credit; one that does is ignored.
            OnionFrame::Credit(_) => return Ok(Vec::new()),
        };
        Ok(self
            .replies
            .accept(now_ms, sequence, frame)?
            .into_iter()
            .map(|frame| self.interpret(frame))
            .collect())
    }

    /// Fail closed if the reply direction's gap has persisted for `V`.
    ///
    /// # Errors
    ///
    /// [`OnionSequenceGap`] then.
    pub(crate) fn expire(&mut self, now_ms: u128) -> Result<(), OnionSequenceGap> {
        self.replies.expire(now_ms)
    }

    /// The meaning of one in-order reply: the first decides the open.
    fn interpret(&mut self, frame: OnionStreamFrame) -> OnionClientEvent {
        let first = !self.decided;
        self.decided = true;
        match (first, frame) {
            (true, OnionStreamFrame::Data { .. }) => OnionClientEvent::Opened,
            (true, OnionStreamFrame::Fin) => OnionClientEvent::Refused,
            (false, OnionStreamFrame::Data { payload, .. }) => OnionClientEvent::Data(payload),
            (false, OnionStreamFrame::Fin) => OnionClientEvent::Fin,
        }
    }

    /// Take the next forward sequence.
    fn advance(&mut self) -> Result<OnionSequence, OnionSequenceExhausted> {
        let sequence = self.forward.ok_or(OnionSequenceExhausted)?;
        self.forward = sequence.next();
        Ok(sequence)
    }
}

/// The client's credit window: the number `W ≤ Q_max` of reply blocks it keeps outstanding at
/// `h` for one session; credit beyond `Q_max` would be dropped at `h`.
///
/// `W` bounds the download rate by `W / RTT`: at the default `W = 64` and a loop round trip below
/// `0.6 s`, one session can take its link's full emission rate (`≈ 98` replies per second, #880).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OnionCreditWindow {
    /// `W`.
    target: usize,
}

impl OnionCreditWindow {
    /// The default window, `W = 64`.
    pub(crate) const DEFAULT: Self = Self { target: 64 };

    /// The window `W = target`.
    #[cfg(test)]
    pub(crate) const fn new(target: usize) -> Self {
        Self { target }
    }

    /// The credit loops to send when `outstanding` blocks are live at `h`, as the number of
    /// blocks each carries in class `class` (see the module diagram): a loop of `j` blocks
    /// leaves `j + 1`, so together they leave at most `min(W, Q_max) − outstanding`, and none
    /// once the window is full. The window is capped at `h`'s pool bound `Q_max`, since a block
    /// past it is dropped at `h` and its loop wasted.
    pub(crate) fn credit_loops(self, outstanding: usize, class: OnionLoopClass) -> Vec<usize> {
        let blocks_per_frame = OnionFrame::credit_capacity(class);
        let deficit = self
            .target
            .min(ONION_SURB_POOL_CAPACITY)
            .saturating_sub(outstanding);
        let full = deficit / (blocks_per_frame + 1);
        let remainder = deficit % (blocks_per_frame + 1);
        let mut loops = vec![blocks_per_frame; full];
        if remainder >= 2 {
            loops.push(remainder - 1);
        }
        loops
    }
}

/// The client's ledger of the reply blocks outstanding at `h` for one session, by expiry.
///
/// ```text
/// sent(t, x, n): L[x] ← L[x] + min(n, Q_max − count(t))                 (h refuses past Q_max)
/// replied(t):   L[x] ← L[x] − 1 for the least x > t with L[x] > 0      (h spends least x first)
/// count(t):     Σ_{x > t} L[x]                                         (h drops a block at x)
/// ```
#[derive(Debug, Default)]
pub(crate) struct OnionClientCredit {
    /// `L`: blocks sent and not yet spent, by expiry.
    outstanding: BTreeMap<OnionExpiry, usize>,
}

impl OnionClientCredit {
    /// Count `count` blocks sent at `now` with expiry `expiry`, of which `h` keeps at most what
    /// its pool has room for.
    pub(crate) fn sent(&mut self, now_ms: u128, expiry: OnionExpiry, count: usize) {
        let room = ONION_SURB_POOL_CAPACITY.saturating_sub(self.count(now_ms));
        let kept = count.min(room);
        if kept > 0 {
            *self.outstanding.entry(expiry).or_default() += kept;
        }
    }

    /// A reply arrived at `now`: `h` spent its block of least expiry.
    pub(crate) fn replied(&mut self, now_ms: u128) {
        self.purge(now_ms);
        if let Some(mut least) = self.outstanding.first_entry() {
            *least.get_mut() -= 1;
            if *least.get() == 0 {
                least.remove();
            }
        }
    }

    /// The blocks still outstanding at `now`.
    pub(crate) fn count(&mut self, now_ms: u128) -> usize {
        self.purge(now_ms);
        self.outstanding.values().sum()
    }

    /// Drop every bucket whose expiry has passed at `now`.
    fn purge(&mut self, now_ms: u128) {
        self.outstanding
            .retain(|expiry, count| *count > 0 && !expiry.has_passed_at(now_ms));
    }
}
