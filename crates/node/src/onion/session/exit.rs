//! The session machine of the world-facing hop `h`: one session `ς`, as a pure transition
//! system whose effects the exit shell performs (#834 D2′, D8; paper Algorithms Hop and Reply).
//!
//! ```text
//! phase:  Unbound ──T-frame, SHA-256(t) = d──▶ Opening ──opened──▶ Open(ack pending)
//!            │  T-frame, SHA-256(t) ≠ d                │ refused        │ υ available
//!            │  or data/fin before any T               ▼                ▼
//!            └────────────────────────────────▶ Closed ◀── fin(0) ── Open(acked)
//!
//! forward(t, f, υ):  Q ← Q ∪ {υ};  f = credit(υ…) ⇒ Q ← Q ∪ υ…;  f ∈ {data, fin} ⇒ reorder,
//!                    then per released frame, in order:
//!   data(t?, w)  Unbound ∧ T ⇒ bind t, Open(t);  Opening ∨ Open ⇒ Write(w) for w ≠ ε
//!                a later T naming another target ⇒ abort
//!   fin          ⇒ ShutdownWrite
//!                    then drain the held world bytes into the new credit
//! opened(ok):        ok ⇒ Open, reply data(0, 0, ε) once some υ is available
//!                    ¬ok ⇒ reply fin(0), Close
//! world(w | eof):    held ← held ‖ w (or eof);  drain
//! drain:             while held ≠ ε ∧ υ = least x exists:  reply data(n, 0, w′), w′ ≤ cap(υ)
//!                    held = ε ∧ eof ∧ υ exists ⇒ reply fin(n)
//! tick(t):           a persisting gap ⇒ abort
//!                    t − last forward ≥ V, or both directions closed ⇒ Close
//! fail(t):           the world failed ⇒ abort
//! abort:             reply fin(n) if a block is left, then Close (fail closed, #834 D2′)
//! ```
//!
//! Laws (tested in `session::tests`):
//!
//! - **Credit.** Every `Reply` effect spends one block taken from `Q` (Invariant Credit), and
//!   [`OnionExitSession::reply_capacity`] is `None` while `Q = ∅` or world bytes are held, so the
//!   shell reads nothing from the world then; a reply never exceeds its own block's capacity.
//! - **Totality.** Every world byte the shell hands in is replied, in order, or the session
//!   aborts: bytes that arrive when every block has expired are held until credit returns, never
//!   dropped (D2′).
//! - **Binding.** `Open(t)` is emitted at most once, and only for `SHA-256(t) = d`; a later `T`
//!   naming another target aborts the session.
//! - **Ack.** The first reply of an opened session is `data(0, 0, ε)`, and a refused one's only
//!   reply is `fin(0)` (#843 Q5); world bytes are never replied before the ack.
//! - **Order.** Forward frames reach the world in sequence order, each once; replies carry
//!   `n = 0, 1, …`.
//! - **Fail closed.** A gap or a world failure spends a remaining block on `fin(n)` before the
//!   session closes, so the client learns of it in one loop instead of by its own timeout.

use std::collections::VecDeque;

use bytes::Bytes;
use rings_core::dht::Did;

use super::frame::OnionFrame;
use super::frame::OnionSequence;
use super::order::OnionReorder;
use super::pool::OnionSurbPool;
use super::OnionTargetDigest;
use crate::onion::circuit::ONION_FORWARD_MAX_VALIDITY_MS;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionSurb;

/// A sequenced frame of a stream direction, as the reorder window releases it.
#[derive(Debug)]
pub(crate) enum OnionStreamFrame {
    /// `data(n, φ, w)`, with `t` while `T` was set.
    Data {
        /// `t`, when `T` was set.
        target: Option<Bytes>,
        /// `w′`.
        payload: Bytes,
    },
    /// `fin(n)`.
    Fin,
}

/// One effect of the exit's session machine, for the shell to perform in order.
#[derive(Debug)]
pub(crate) enum OnionExitEffect {
    /// Connect or prepare the world at `target`, the canonical authority bytes of `t`.
    Open {
        /// `t`.
        target: Bytes,
    },
    /// Write stream bytes to the world.
    Write(Bytes),
    /// The client closed its direction: shut the world's write half.
    ShutdownWrite,
    /// Send a produced reply cell to `next`, the first hop of its return path.
    Reply {
        /// The DID the reply cell goes to.
        next: Did,
        /// The reply cell.
        cell: OnionCell,
    },
    /// Drop the session and release the world.
    Close,
}

/// The phase of a session at `h`; see the module diagram.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OnionExitPhase {
    /// No `T` frame with the right digest yet.
    Unbound,
    /// The world is being opened.
    Opening,
    /// The world is open; `acked` once `data(0, 0, ε)` has been replied.
    Open {
        /// Whether the open ack has left.
        acked: bool,
    },
    /// The session is over; every later input is ignored.
    Closed,
}

/// What the world handed the session: bytes, or the end of its stream.
#[derive(Debug)]
pub(crate) enum OnionWorldRead {
    /// Up to [`OnionExitSession::reply_capacity`] bytes.
    Bytes(Bytes),
    /// The world closed its write half.
    Eof,
}

/// The session machine of one `ς` at `h`; see the module documentation.
#[derive(Debug)]
pub(crate) struct OnionExitSession {
    /// `d`, the digest `ā` bound the session to.
    digest: OnionTargetDigest,
    /// Where the session stands.
    phase: OnionExitPhase,
    /// The client-to-`h` direction.
    forward: OnionReorder<OnionStreamFrame>,
    /// `Q_{h,ς}`.
    pool: OnionSurbPool,
    /// The sequence of the next reply; `None` once the 32-bit space is spent.
    reply: Option<OnionSequence>,
    /// World bytes read and not yet replied, in order.
    held: VecDeque<Bytes>,
    /// Whether the world's stream has ended and its `fin` is not yet replied.
    eof_held: bool,
    /// Whether the world's stream has ended (`fin` replied).
    read_closed: bool,
    /// Whether the client's stream has ended (`fin` applied).
    write_closed: bool,
    /// The arrival of the last forward loop.
    last_forward_ms: u128,
}

impl OnionExitSession {
    /// A session for the digest `d` of `ā`, created by its first forward loop at `now`.
    pub(crate) fn new(digest: OnionTargetDigest, now_ms: u128) -> Self {
        Self {
            digest,
            phase: OnionExitPhase::Unbound,
            forward: OnionReorder::default(),
            pool: OnionSurbPool::default(),
            reply: Some(OnionSequence::FIRST),
            held: VecDeque::new(),
            eof_held: false,
            read_closed: false,
            write_closed: false,
            last_forward_ms: now_ms,
        }
    }

    /// Whether the session has closed; the shell then drops it.
    pub(crate) fn is_closed(&self) -> bool {
        self.phase == OnionExitPhase::Closed
    }

    /// One forward loop at `now`: its reply block and its frame.
    pub(crate) fn forward(
        &mut self,
        now_ms: u128,
        frame: OnionFrame,
        surb: OnionSurb,
    ) -> Vec<OnionExitEffect> {
        if self.is_closed() {
            return Vec::new();
        }
        self.last_forward_ms = now_ms;
        // A block the pool refuses (full, or expired) is credit beyond `Q_max`: dropped (D8).
        self.pool.add(now_ms, surb);
        let mut effects = Vec::new();
        match frame {
            OnionFrame::Credit(surbs) => surbs.into_iter().for_each(|surb| {
                self.pool.add(now_ms, surb);
            }),
            OnionFrame::Data {
                sequence,
                target,
                payload,
            } => self.release(
                now_ms,
                sequence,
                OnionStreamFrame::Data { target, payload },
                &mut effects,
            ),
            OnionFrame::Fin { sequence } => {
                self.release(now_ms, sequence, OnionStreamFrame::Fin, &mut effects);
            }
        }
        self.flush_ack(now_ms, &mut effects);
        self.drain(now_ms, &mut effects);
        effects
    }

    /// The world at `target` was opened (`true`) or refused (`false`) at `now`.
    pub(crate) fn opened(&mut self, now_ms: u128, opened: bool) -> Vec<OnionExitEffect> {
        let mut effects = Vec::new();
        if self.phase != OnionExitPhase::Opening {
            return effects;
        }
        if opened {
            self.phase = OnionExitPhase::Open { acked: false };
            self.flush_ack(now_ms, &mut effects);
        } else {
            self.reply_frame(now_ms, ReplyKind::Fin, &mut effects);
            self.close(&mut effects);
        }
        effects
    }

    /// The most world bytes one reply can carry now: the data capacity of the block it would
    /// spend, or `None` while the session may not read the world (not acked, the world's stream
    /// ended, world bytes still held, or `Q_{h,ς} = ∅`, Invariant Credit).
    pub(crate) fn reply_capacity(&mut self, now_ms: u128) -> Option<usize> {
        match self.phase {
            OnionExitPhase::Open { acked: true }
                if !self.read_closed && !self.eof_held && self.held.is_empty() =>
            {
                self.pool.least_class(now_ms).map(OnionFrame::data_capacity)
            }
            _ => None,
        }
    }

    /// The world handed the session `read` at `now`: it is held, then drained into the credit
    /// there is (Law Totality).
    pub(crate) fn world(&mut self, now_ms: u128, read: OnionWorldRead) -> Vec<OnionExitEffect> {
        let mut effects = Vec::new();
        if !matches!(self.phase, OnionExitPhase::Open { acked: true }) || self.read_closed {
            return effects;
        }
        match read {
            OnionWorldRead::Bytes(bytes) if bytes.is_empty() => {}
            OnionWorldRead::Bytes(bytes) => self.held.push_back(bytes),
            OnionWorldRead::Eof => self.eof_held = true,
        }
        self.drain(now_ms, &mut effects);
        effects
    }

    /// The periodic step at `now`: a persisting gap aborts; `V` without a forward loop, or both
    /// directions closed, closes the session.
    pub(crate) fn tick(&mut self, now_ms: u128) -> Vec<OnionExitEffect> {
        let mut effects = Vec::new();
        if self.is_closed() {
            return effects;
        }
        if self.forward.expire(now_ms).is_err() {
            self.abort(now_ms, &mut effects);
        } else if now_ms.saturating_sub(self.last_forward_ms) >= ONION_FORWARD_MAX_VALIDITY_MS
            || (self.read_closed && self.write_closed)
        {
            self.close(&mut effects);
        }
        effects
    }

    /// The world failed at `now` (a read, a write, or the byte policy): the session aborts.
    pub(crate) fn fail(&mut self, now_ms: u128) -> Vec<OnionExitEffect> {
        let mut effects = Vec::new();
        self.abort(now_ms, &mut effects);
        effects
    }

    /// Accept one sequenced forward frame and apply every frame it releases, in order.
    fn release(
        &mut self,
        now_ms: u128,
        sequence: OnionSequence,
        frame: OnionStreamFrame,
        effects: &mut Vec<OnionExitEffect>,
    ) {
        match self.forward.accept(now_ms, sequence, frame) {
            Ok(released) => released
                .into_iter()
                .for_each(|frame| self.apply(now_ms, frame, effects)),
            Err(_) => self.abort(now_ms, effects),
        }
    }

    /// Apply one in-order forward frame at `now` (see the module diagram).
    fn apply(&mut self, now_ms: u128, frame: OnionStreamFrame, effects: &mut Vec<OnionExitEffect>) {
        match (self.phase, frame) {
            (OnionExitPhase::Closed, _) => {}
            (
                OnionExitPhase::Unbound,
                OnionStreamFrame::Data {
                    target: Some(target),
                    payload,
                },
            ) if OnionTargetDigest::of(&target) == self.digest => {
                self.phase = OnionExitPhase::Opening;
                effects.push(OnionExitEffect::Open { target });
                Self::write(payload, effects);
            }
            // A digest mismatch, or stream bytes before the target is known, is not a session.
            (OnionExitPhase::Unbound, _) => self.close(effects),
            // Law Binding: a bound session never takes another target.
            (
                _,
                OnionStreamFrame::Data {
                    target: Some(target),
                    ..
                },
            ) if OnionTargetDigest::of(&target) != self.digest => self.abort(now_ms, effects),
            (_, OnionStreamFrame::Data { payload, .. }) => Self::write(payload, effects),
            (_, OnionStreamFrame::Fin) => {
                self.write_closed = true;
                effects.push(OnionExitEffect::ShutdownWrite);
            }
        }
    }

    /// Emit `Write(w)` for a non-empty `w`: an empty payload is a credit-only loop.
    fn write(payload: Bytes, effects: &mut Vec<OnionExitEffect>) {
        if !payload.is_empty() {
            effects.push(OnionExitEffect::Write(payload));
        }
    }

    /// Reply the open ack `data(0, 0, ε)` if it is pending and a block is available.
    fn flush_ack(&mut self, now_ms: u128, effects: &mut Vec<OnionExitEffect>) {
        if self.phase == (OnionExitPhase::Open { acked: false }) && !self.pool.is_empty(now_ms) {
            self.phase = OnionExitPhase::Open { acked: true };
            self.reply_frame(now_ms, ReplyKind::Data(Bytes::new()), effects);
        }
    }

    /// Reply the held world bytes, each block carrying at most its own capacity, and then the
    /// held end of stream, while blocks last (see the module diagram).
    fn drain(&mut self, now_ms: u128, effects: &mut Vec<OnionExitEffect>) {
        if !matches!(self.phase, OnionExitPhase::Open { acked: true }) {
            return;
        }
        while let Some(capacity) = self.pool.least_class(now_ms).map(OnionFrame::data_capacity) {
            if self.is_closed() {
                return;
            }
            let Some(front) = self.held.front_mut() else {
                break;
            };
            let chunk = front.split_to(capacity.min(front.len()));
            if front.is_empty() {
                self.held.pop_front();
            }
            self.reply_frame(now_ms, ReplyKind::Data(chunk), effects);
        }
        if self.held.is_empty() && self.eof_held && !self.pool.is_empty(now_ms) {
            self.eof_held = false;
            self.read_closed = true;
            self.reply_frame(now_ms, ReplyKind::Fin, effects);
            if self.write_closed {
                self.close(effects);
            }
        }
    }

    /// Fail closed: reply `fin(n)` if a block is left, then close.
    fn abort(&mut self, now_ms: u128, effects: &mut Vec<OnionExitEffect>) {
        if self.is_closed() {
            return;
        }
        if self.reply.is_some() && !self.pool.is_empty(now_ms) {
            self.reply_frame(now_ms, ReplyKind::Fin, effects);
        }
        self.close(effects);
    }

    /// Spend the block of least expiry on one reply frame, with the next reply sequence. With no
    /// block, no sequence left, or a failed production, the session closes: a reply that cannot
    /// leave would be a gap at the client.
    fn reply_frame(&mut self, now_ms: u128, kind: ReplyKind, effects: &mut Vec<OnionExitEffect>) {
        let (Some(sequence), Some(surb)) = (self.reply, self.pool.take(now_ms)) else {
            self.close(effects);
            return;
        };
        let frame = match kind {
            ReplyKind::Data(payload) => OnionFrame::Data {
                sequence,
                target: None,
                payload,
            },
            ReplyKind::Fin => OnionFrame::Fin { sequence },
        };
        // A data frame is cut to its own block's capacity (`drain`), so `encode` refuses
        // nothing here; a weak key of the block has probability 2^−124.
        let produced = frame
            .encode(surb.class())
            .ok()
            .and_then(|value| surb.produce(&value).ok());
        match produced {
            Some((next, cell)) => {
                self.reply = sequence.next();
                effects.push(OnionExitEffect::Reply { next, cell });
            }
            None => self.close(effects),
        }
    }

    /// Enter `Closed` and tell the shell, once.
    fn close(&mut self, effects: &mut Vec<OnionExitEffect>) {
        if !self.is_closed() {
            self.phase = OnionExitPhase::Closed;
            effects.push(OnionExitEffect::Close);
        }
    }
}

/// The reply a block is spent on.
enum ReplyKind {
    /// `data(n, 0, w)`.
    Data(Bytes),
    /// `fin(n)`.
    Fin,
}
