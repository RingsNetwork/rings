//! The client's session machine and credit window (#834 D2′, D6′, D8).
//!
//! ```text
//! data(w):   frame data(n, T?, w), n ← n + 1;  T is set, with t inline, until the first reply
//! fin:       frame fin(n), n ← n + 1
//! reply(f):  replied ← ⊤;  reorder f, then per released frame, in order:
//!   first frame   data(0, ε) ⇒ Opened      fin ⇒ Refused (a fin before any data: no reason given)
//!   later frames  data(w)    ⇒ Data(w)     fin ⇒ Fin
//! credit:    want W reply blocks outstanding at h; each forward loop leaves one and each credit
//!            loop k + 1, so the deficit W − outstanding asks ⌈deficit / (k + 1)⌉ credit loops
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
//! - **Credit.** The window never asks for more than `W ≤ Q_max` outstanding blocks.

use bytes::Bytes;

use super::exit::OnionStreamFrame;
use super::frame::OnionFrame;
use super::frame::OnionSequence;
use super::order::OnionReorder;
use super::order::OnionSequenceGap;
use super::pool::ONION_SURB_POOL_CAPACITY;
use super::OnionSessionArguments;
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
    /// `ā = (ς, d)`.
    arguments: OnionSessionArguments,
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
    /// A session of `arguments` whose target has the canonical authority `target`, with
    /// `arguments.digest = SHA-256(target)`.
    pub(crate) fn new(arguments: OnionSessionArguments, target: Bytes) -> Self {
        Self {
            arguments,
            target,
            forward: Some(OnionSequence::FIRST),
            replied: false,
            decided: false,
            replies: OnionReorder::default(),
        }
    }

    /// `ā`, the same on every loop of the session.
    pub(crate) const fn arguments(&self) -> OnionSessionArguments {
        self.arguments
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
/// `h` for one session.
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

    /// A window of `target` blocks, capped at `Q_max`: `h` drops credit beyond its pool.
    pub(crate) const fn new(target: usize) -> Self {
        Self {
            target: if target < ONION_SURB_POOL_CAPACITY {
                target
            } else {
                ONION_SURB_POOL_CAPACITY
            },
        }
    }

    /// The credit loops to send when `outstanding` blocks are live at `h`, each leaving
    /// `k + 1` blocks in class `class`: `⌈(W − outstanding) / (k + 1)⌉`, and none once the window
    /// is full.
    pub(crate) fn loops_wanted(self, outstanding: usize, class: OnionLoopClass) -> usize {
        let per_loop = OnionFrame::credit_capacity(class) + 1;
        self.target.saturating_sub(outstanding).div_ceil(per_loop)
    }
}
