//! The session machine of the world-facing hop `h`: one session `ς`, as a pure transition
//! system whose effects the exit shell performs (#834 D2′, D8; paper Algorithms Hop and Reply).
//!
//! ```text
//! phase:  Unbound ──T-frame, SHA-256(t) = d──▶ Opening ──opened──▶ Open(ack pending)
//!            │  T-frame, SHA-256(t) ≠ d                │ refused        │ υ available
//!            │  or data/fin before any T               ▼                ▼
//!            │  (no reply; the tombstone answers       │                │
//!            │   the next loop with abort(0))          │                │
//!            └────────────────────────────────▶ Closed ◀─ abort(0) ─ Open(acked)
//! (a T-frame is a `data` or a `fin` carrying T)
//!
//! forward(t, f, υ):  Q ← Q ∪ {υ};  f = credit(m, υ…) ⇒ Q ← Q ∪ υ…, then mark m (a gap ⇒ abort);
//!                    f ∈ {data, fin} ⇒ reorder, then per released frame, in order:
//!   data(t?, w)  Unbound ∧ T ⇒ bind t, Open(t);  Opening ∨ Open ⇒ Write(w) for w ≠ ε
//!                a later T naming another target ⇒ abort
//!   fin(t?)      Unbound ∧ T ⇒ bind t, Open(t), ShutdownWrite (an empty upload);
//!                otherwise ⇒ ShutdownWrite;  a later T naming another target ⇒ abort
//!                    then drain the held world bytes into the new credit,
//!                    then echo
//! forward abort(n):  the client gave up ⇒ Close, on arrival (no reply)
//! opened(ok):        ok ⇒ Open, reply data(0, 0, ε) once some υ is available
//!                    ¬ok ⇒ abort
//! world(w | eof):    held ← held ‖ w (or eof);  drain
//! drain:             while held ≠ ε ∧ υ = least x exists:  reply data(n, 0, w′), w′ ≤ cap(υ)
//!                    held = ε ∧ eof ∧ υ exists ⇒ reply fin(n)
//! tick(t):           Unbound for Q since the first loop ⇒ Close (no world was opened)
//!                    a persisting gap ⇒ abort
//!                    t − last forward ≥ V, or both directions closed ⇒ Close
//! fail(t):           the world failed ⇒ abort
//! echo(t):           Open(acked) ∧ ¬read_closed ∧ t − last_reply ≥ V/2 ⇒ reply data(n, 0, ε)
//!                    (last_reply: when the last reply of ς left; every reply sets it)
//! abort:             reply abort(n) if a block is left, then Close (fail closed, #834 D2′);
//!                    held bytes are dropped, since the stream ends failed; after the world's
//!                    fin is replied, Close without a reply (no abort overtakes that fin)
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
//!   reply is `abort(0)`: a refusal is a failure (the `abort` decision of #834 D2′, which
//!   supersedes #843 Q5's refusing `fin`); world bytes are never replied before the ack.
//! - **Order.** Forward frames reach the world in sequence order, each once; replies carry
//!   `n = 0, 1, …`.
//! - **Fail closed.** A gap, a world failure, a spent byte policy or a rebind spends a remaining
//!   block on `abort(n)` before the session closes, so the client learns of it in one loop, as
//!   a failure; `fin` is sent only when the world's bytes really ended. Without a block the
//!   session closes without a reply, and the shell answers the client's next loop, which
//!   brings its own block, with `abort` ([`reply_to_unheld`]).
//! - **Mark** (paper, D2′ Order; #981). A `credit(m, υ…)` whose `m` exceeds the next expected
//!   forward sequence makes that frame known missing from the credit's arrival, exactly as a
//!   later sequenced frame would ([`OnionReorder::mark`]); the gap rule then aborts `ς` after
//!   `V`, under a block the credit loops left. `m` never releases or invents a frame, and an
//!   `m` at or below the next expected sequence changes nothing. With the client's keep-alive,
//!   a forward frame sent at `t₀` and lost on a delivering route is followed by a loop carrying
//!   a sequence or a mark above it, arriving by `t₀ + V/2 + F` (`F` one forward-segment
//!   latency), so `h` aborts `ς` by `t₀ + 3V/2 + F`, whether or not a later sequenced frame
//!   follows (paper, Prop. Liveness of failure (iii)).
//! - **Echo** (D2′ Liveness). A forward loop arriving at `a` when no reply of `ς` has left in
//!   `(a − V/2, a]` is answered at once with the echo `data(n, 0, ε)`. The echo spends the
//!   block that loop brought, so it is credit-neutral (the arrival changes `|Q_{h,ς}|` by the
//!   blocks its frame carries alone), consumes the next reply sequence, and carries no stream
//!   bytes. The rule is a pure function of the arrival instant and the last reply's departure;
//!   `h` keeps no timer for it, and a burst of forward loops costs at most one echo per `V/2`.
//!   No echo precedes the open ack, so `data(0, 0, ε)` and `abort(0)` keep their meaning, and
//!   none follows the world's `fin`. With the client's keep-alive, which sends a forward loop
//!   whenever none has left for `V/2`, forward arrivals are less than `3V/4` apart when segment
//!   latency varies by less than `V/4`, so after a reply at `r` some arrival in
//!   `[r + V/2, r + 5V/4)` finds the horizon empty: replies of `ς` leave `h` less than `5V/4`
//!   apart, and reach the client less than `3V/2 < Δ = 2V` apart (paper, Prop. Liveness of
//!   failure).

use std::collections::VecDeque;

use bytes::Bytes;
use rings_core::dht::Did;

use super::frame::OnionFrame;
use super::frame::OnionSequence;
use super::order::OnionReorder;
use super::pool::OnionSurbPool;
use super::OnionTargetDigest;
use crate::onion::circuit::ONION_ADMISSION_WINDOW_MS;
use crate::onion::circuit::ONION_EXPIRY_QUANTUM_MS;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionSurb;

/// `V/2`, the echo horizon: a forward loop is echoed when no reply of its session has left for
/// this long (Law Echo). It equals the client's keep-alive period, so an idle session costs one
/// echo per keep-alive.
pub(crate) const ONION_SESSION_ECHO_HORIZON_MS: u128 = ONION_ADMISSION_WINDOW_MS / 2;

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
    /// `fin(n)`, with `t` while `T` was set.
    Fin {
        /// `t`, when `T` was set.
        target: Option<Bytes>,
    },
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
    /// Seal `frame` into the reply cell of `surb` ([`seal`]) and send it to the first hop of
    /// its return path: the block is spent, whatever becomes of the cell.
    Reply {
        /// The reply frame.
        frame: OnionFrame,
        /// The block it is sealed under, boxed: a block is far wider than every other effect.
        surb: Box<OnionSurb>,
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
    /// The arrival of the first forward loop.
    created_ms: u128,
    /// The departure of the last reply, the echo rule's state; meaningful once acked, since
    /// the ack is the first reply.
    last_reply_ms: u128,
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
            created_ms: now_ms,
            last_reply_ms: now_ms,
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
            OnionFrame::Credit { mark, surbs } => {
                surbs.into_iter().for_each(|surb| {
                    self.pool.add(now_ms, surb);
                });
                // Law Mark: the blocks are pooled first, so an abort can spend one of them.
                if self.forward.mark(now_ms, mark).is_err() {
                    self.abort(now_ms, &mut effects);
                }
            }
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
            OnionFrame::Fin { sequence, target } => {
                self.release(
                    now_ms,
                    sequence,
                    OnionStreamFrame::Fin { target },
                    &mut effects,
                );
            }
            // The client gave the session up: stop reading the world at once.
            OnionFrame::Abort { .. } => {
                self.close(&mut effects);
                return effects;
            }
        }
        self.flush_ack(now_ms, &mut effects);
        self.drain(now_ms, &mut effects);
        self.echo(now_ms, &mut effects);
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
            self.abort(now_ms, &mut effects);
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

    /// The periodic step at `now`: a session still unbound `Q` after its first loop closes (it
    /// holds a table slot and a lease but opened nothing); a persisting gap aborts; `V` without a
    /// forward loop, or both directions closed, closes the session.
    pub(crate) fn tick(&mut self, now_ms: u128) -> Vec<OnionExitEffect> {
        let mut effects = Vec::new();
        if self.is_closed() {
            return effects;
        }
        if self.phase == OnionExitPhase::Unbound
            && now_ms.saturating_sub(self.created_ms) >= ONION_EXPIRY_QUANTUM_MS
        {
            self.close(&mut effects);
        } else if self.forward.expire(now_ms).is_err() {
            self.abort(now_ms, &mut effects);
        } else if now_ms.saturating_sub(self.last_forward_ms) >= ONION_ADMISSION_WINDOW_MS
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
            // A session whose first frame is its `fin`: it opens, and its upload is empty.
            (
                OnionExitPhase::Unbound,
                OnionStreamFrame::Fin {
                    target: Some(target),
                },
            ) if OnionTargetDigest::of(&target) == self.digest => {
                self.phase = OnionExitPhase::Opening;
                effects.push(OnionExitEffect::Open { target });
                self.write_closed = true;
                effects.push(OnionExitEffect::ShutdownWrite);
            }
            // A digest mismatch, or stream bytes before the target is known, is not a session.
            (OnionExitPhase::Unbound, _) => self.close(effects),
            // Law Binding: a bound session never takes another target.
            (
                _,
                OnionStreamFrame::Data {
                    target: Some(target),
                    ..
                }
                | OnionStreamFrame::Fin {
                    target: Some(target),
                },
            ) if OnionTargetDigest::of(&target) != self.digest => self.abort(now_ms, effects),
            (_, OnionStreamFrame::Data { payload, .. }) => Self::write(payload, effects),
            (_, OnionStreamFrame::Fin { .. }) => {
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
            self.reply_frame(
                now_ms,
                |sequence| OnionFrame::Data {
                    sequence,
                    target: None,
                    payload: Bytes::new(),
                },
                effects,
            );
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
            self.reply_frame(
                now_ms,
                |sequence| OnionFrame::Data {
                    sequence,
                    target: None,
                    payload: chunk,
                },
                effects,
            );
        }
        if self.held.is_empty() && self.eof_held && !self.pool.is_empty(now_ms) {
            self.eof_held = false;
            self.read_closed = true;
            self.reply_frame(
                now_ms,
                |sequence| OnionFrame::Fin {
                    sequence,
                    target: None,
                },
                effects,
            );
            if self.write_closed {
                self.close(effects);
            }
        }
    }

    /// The echo rule at the forward arrival `now` (Law Echo): if no reply has left in
    /// `(now − V/2, now]`, reply `data(n, 0, ε)` under the block the loop brought. Nothing before
    /// the ack, after the world's `fin`, or with `Q_{h,ς} = ∅` (the loop's block was refused),
    /// since a reply that cannot leave would close the session.
    fn echo(&mut self, now_ms: u128, effects: &mut Vec<OnionExitEffect>) {
        let quiet = self.phase == (OnionExitPhase::Open { acked: true })
            && !self.read_closed
            && now_ms.saturating_sub(self.last_reply_ms) >= ONION_SESSION_ECHO_HORIZON_MS
            && !self.pool.is_empty(now_ms);
        if quiet {
            self.reply_frame(
                now_ms,
                |sequence| OnionFrame::Data {
                    sequence,
                    target: None,
                    payload: Bytes::new(),
                },
                effects,
            );
        }
    }

    /// Fail closed: reply `abort(n)` if a block is left, then close; held bytes are dropped,
    /// since nothing after an `abort` counts. Once the world's `fin` is replied the download is
    /// complete, so the session closes without an `abort` that could overtake that `fin`; a
    /// later loop is answered as unheld.
    fn abort(&mut self, now_ms: u128, effects: &mut Vec<OnionExitEffect>) {
        if self.is_closed() {
            return;
        }
        if self.read_closed {
            self.close(effects);
            return;
        }
        self.held.clear();
        self.eof_held = false;
        self.reply_frame(now_ms, |sequence| OnionFrame::Abort { sequence }, effects);
        self.close(effects);
    }

    /// Spend the block of least expiry on one reply frame, which `frame` builds from the next
    /// reply sequence. With no block or no sequence left, the session closes: a reply that cannot
    /// leave would be a gap at the client.
    fn reply_frame(
        &mut self,
        now_ms: u128,
        frame: impl FnOnce(OnionSequence) -> OnionFrame,
        effects: &mut Vec<OnionExitEffect>,
    ) {
        let (Some(sequence), Some(surb)) = (self.reply, self.pool.take(now_ms)) else {
            self.close(effects);
            return;
        };
        let frame = frame(sequence);
        self.reply = sequence.next();
        self.last_reply_ms = now_ms;
        effects.push(OnionExitEffect::Reply {
            frame,
            surb: Box::new(surb),
        });
    }

    /// Enter `Closed` and tell the shell, once.
    fn close(&mut self, effects: &mut Vec<OnionExitEffect>) {
        if !self.is_closed() {
            self.phase = OnionExitPhase::Closed;
            effects.push(OnionExitEffect::Close);
        }
    }
}

/// Seal `frame` into the reply cell of `surb`: the cell and the DID it goes to, the first hop of
/// its return path.
///
/// `None` for a frame wider than the block's class, which the machine never replies (a data
/// frame is cut to its own block's capacity in `drain`), or for a weak key of the block, of
/// probability `2^−124`; the shell then fails the session closed.
pub(crate) fn seal(frame: &OnionFrame, surb: OnionSurb) -> Option<(Did, OnionCell)> {
    let value = frame.encode(surb.class()).ok()?;
    surb.produce(value.as_slice()).ok()
}

/// The answer of `h` to a forward loop of a session it does not hold (closed, refused, or never
/// opened), sealed under that loop's own block: `abort`, so a client still using the session
/// learns within one loop that it is gone. A client's own `abort` is answered by
/// nothing, since that client already gave the session up.
///
/// The answer's `n` is `0`: `abort` is applied on arrival and its `n` is informational, and `h`
/// keeps no sequence of a session it does not hold. Every loop brings exactly one block, so the
/// answer is one reply per forward loop and amplifies nothing.
pub(crate) fn reply_to_unheld(frame: &OnionFrame) -> Option<OnionFrame> {
    match frame {
        OnionFrame::Abort { .. } => None,
        OnionFrame::Data { .. } | OnionFrame::Fin { .. } | OnionFrame::Credit { .. } => {
            Some(OnionFrame::Abort {
                sequence: OnionSequence::FIRST,
            })
        }
    }
}
