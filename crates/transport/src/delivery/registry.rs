//! Pure per-channel registry of pending delivery confirmations.
//!
//! A data channel exposes one scalar, `bufferedAmount`, and one edge-triggered
//! event: `bufferedamountlow` fires when the buffered amount falls from above
//! the channel's single `bufferedAmountLowThreshold` to at or below it. Many
//! sends can wait on one channel at once, so this registry multiplexes that
//! one threshold over every pending end offset.
//!
//! # Model
//!
//! Let `E` be the channel's enqueued-byte counter and `b` its buffered amount.
//! A send whose bytes end at offset `e` has left the local buffer exactly when
//! the flush predicate `φ(E, b, e) ≜ E ⊖ b ≥ e` holds (`⊖` is saturating
//! subtraction, see [`delivery_flushed`]). The registry is the state
//!
//! ```text
//! slots : Ticket ⇀ Waiting(e, waker) | Settled(Flushed | Closed)
//! closed : 𝔹          round : Idle | Running | Rerun
//! ```
//!
//! and the threshold it arms for the earliest waiting offset is
//!
//! ```text
//! τ(E) ≜ E ⊖ min { e | Waiting(e, _) ∈ slots }
//! ```
//!
//! so that `b ≤ τ(E) ⇔ φ(E, b, e_min)`: the event fires exactly when the
//! earliest pending send leaves the buffer.
//!
//! # Settle round
//!
//! A round is the only writer of the channel threshold, and at most one round
//! runs per channel (`round ≠ Idle` is its lock, a single-flight flag):
//!
//! ```text
//! loop
//!   τ ← begin_step(E)          -- None ⇒ round := Idle; stop
//!   arm(τ)                     -- effect: set the channel threshold
//!   b ← observe()              -- effect: read bufferedAmount AFTER arming
//!   settle(E, b)               -- Waiting(e) ∧ φ(E, b, e) ↦ Settled(Flushed)
//!   if nothing settled ∧ round = Running then round := Idle; stop
//! ```
//!
//! # Invariants (TLA+ style)
//!
//! ```text
//! Soundness  ≜ □ (Settled(Flushed) ∈ slots[t] ⇒ φ(E, b, e_t) held at some read)
//! Closure    ≜ □ (closed ⇒ ∀ t. slots[t] ∉ Waiting)
//! Armed      ≜ □ (round = Idle ∧ ∃ Waiting ⇒ b_read > τ_armed)
//! Liveness   ≜ □ (φ(E, b, e_t) ∧ Waiting(e_t) ∧ round = Idle
//!                  ⇒ ◇ low-event ∨ ◇ request_round)
//! ```
//!
//! `Armed` is the lost-wakeup rule. A round arms `τ` BEFORE it reads `b`; a
//! quiescent step settled nothing, so `E ⊖ b < e_min`, that is `b > τ`. The
//! next drain across `τ` is therefore a crossing the channel reports. A drain
//! that completed before the arm is covered by the read that follows it. Any
//! request (event or registration) that arrives while a round runs turns
//! `Running` into `Rerun`, which forces one more arm-and-read step, so no
//! request is absorbed by a round that had already read `b`.
//!
//! Every enqueue advances `E` and therefore raises `τ`; the backend follows
//! each successful enqueue with a registration, whose round re-arms `τ`
//! against the new `E`.
//!
//! Wakers are data here: the registry returns them and the shell wakes them
//! after releasing its lock. No clock takes part in any verdict.

use std::collections::BTreeMap;
use std::task::Poll;
use std::task::Waker;

use super::delivery_flushed;

/// Identity of one registered send inside its channel's registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Ticket(u64);

/// Terminal verdict of one registered send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The send's end offset left the local buffer: `φ(E, b, e)` held.
    Flushed,
    /// The channel closed or failed before the send was observed flushed.
    Closed,
}

/// Lifecycle of one registered send.
#[derive(Debug)]
enum Slot {
    /// Still buffered; `waker` is the latest waiter, if the send was polled.
    Waiting {
        /// Cumulative end offset of the send's bytes on this channel.
        end_offset: u64,
        /// Waker of the most recent poll, absent before the first poll.
        waker: Option<Waker>,
    },
    /// Decided; the next poll reports it and removes the slot.
    Settled(Verdict),
}

impl Slot {
    /// Return the end offset of a waiting slot, `None` once settled.
    fn waiting_offset(&self) -> Option<u64> {
        match self {
            Self::Waiting { end_offset, .. } => Some(*end_offset),
            Self::Settled(_) => None,
        }
    }

    /// Settle a waiting slot with `verdict`, returning the waker to notify.
    ///
    /// A settled verdict is final: a flush observed before a close stays `Flushed`.
    fn settle(&mut self, verdict: Verdict) -> Option<Waker> {
        let Self::Waiting { waker, .. } = self else {
            return None;
        };
        let waker = waker.take();
        *self = Self::Settled(verdict);
        waker
    }
}

/// Phase of the channel's single-flight settle round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Round {
    /// No round runs; the next request starts one.
    #[default]
    Idle,
    /// One round runs and has seen every request so far.
    Running,
    /// One round runs and a request arrived after its last read of `b`.
    Rerun,
}

/// Whether a settle round must take another arm-and-read step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Something changed since the last arm; arm again and re-read.
    Continue,
    /// The armed threshold is below the observed buffer: wait for the event.
    Quiescent,
}

/// Outcome of one observation: the wakers to notify and the next step.
#[derive(Debug)]
pub(crate) struct Settlement {
    /// Waiters whose sends were settled by this observation.
    pub(crate) wakers: Vec<Waker>,
    /// Whether the round continues.
    pub(crate) step: Step,
}

/// The pure registry of one data channel. See the module documentation.
#[derive(Debug, Default)]
pub(crate) struct DeliveryRegistry {
    /// Registered sends by ticket; tickets grow with registration order.
    slots: BTreeMap<Ticket, Slot>,
    /// The next ticket to issue.
    next_ticket: u64,
    /// Sticky: the channel closed or failed; later sends settle `Closed`.
    closed: bool,
    /// Phase of the single-flight settle round.
    round: Round,
}

impl DeliveryRegistry {
    /// Register a send ending at `end_offset`. After a close it is born `Closed`.
    pub(crate) fn register(&mut self, end_offset: u64) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket = self.next_ticket.saturating_add(1);
        let slot = if self.closed {
            Slot::Settled(Verdict::Closed)
        } else {
            Slot::Waiting {
                end_offset,
                waker: None,
            }
        };
        self.slots.insert(ticket, slot);
        ticket
    }

    /// Report a settled verdict (removing its slot) or record `waker` for later.
    ///
    /// A ticket without a slot was already reported; it answers `Closed` so a
    /// misuse can never fabricate a flush.
    pub(crate) fn poll(&mut self, ticket: Ticket, waker: &Waker) -> Poll<Verdict> {
        match self.slots.get_mut(&ticket) {
            None => Poll::Ready(Verdict::Closed),
            Some(Slot::Settled(verdict)) => {
                let verdict = *verdict;
                self.slots.remove(&ticket);
                Poll::Ready(verdict)
            }
            Some(Slot::Waiting {
                waker: registered, ..
            }) => {
                match registered {
                    Some(current) if current.will_wake(waker) => {}
                    _ => *registered = Some(waker.clone()),
                }
                Poll::Pending
            }
        }
    }

    /// Drop a send whose waiter went away. Its offset no longer bounds `τ`.
    pub(crate) fn forget(&mut self, ticket: Ticket) {
        self.slots.remove(&ticket);
    }

    /// Record a channel close or failure: every waiting send settles `Closed`.
    pub(crate) fn close(&mut self) -> Vec<Waker> {
        self.closed = true;
        self.slots
            .values_mut()
            .filter_map(|slot| slot.settle(Verdict::Closed))
            .collect()
    }

    /// Ask for a settle round. `true` iff the caller must start one now.
    ///
    /// `Idle → Running` hands the round to the caller; a running round is
    /// marked `Rerun` so it takes one more arm-and-read step.
    pub(crate) fn request_round(&mut self) -> bool {
        let start = self.round == Round::Idle;
        self.round = if start { Round::Running } else { Round::Rerun };
        start
    }

    /// Begin one step of the running round: the threshold `τ(E)` to arm.
    ///
    /// `None` means nothing waits; the round ends and the phase is `Idle`.
    /// Pending requests are absorbed here because this step re-reads all state.
    pub(crate) fn begin_step(&mut self, enqueued: u64) -> Option<u64> {
        let earliest = self.slots.values().filter_map(Slot::waiting_offset).min();
        self.round = if earliest.is_some() {
            Round::Running
        } else {
            Round::Idle
        };
        earliest.map(|end_offset| enqueued.saturating_sub(end_offset))
    }

    /// Apply one observation `(E, b)` read after arming: settle every flushed send.
    ///
    /// The round ends (`Quiescent`, phase `Idle`) only when this observation
    /// settled nothing and no request arrived since the step began.
    pub(crate) fn settle(&mut self, enqueued: u64, buffered: u64) -> Settlement {
        let flushed: Vec<&mut Slot> = self
            .slots
            .values_mut()
            .filter(|slot| {
                slot.waiting_offset()
                    .is_some_and(|end_offset| delivery_flushed(enqueued, buffered, end_offset))
            })
            .collect();
        // A settled slot moves `e_min`, so `τ` must be re-armed even if unpolled.
        let progressed = !flushed.is_empty();
        let wakers = flushed
            .into_iter()
            .filter_map(|slot| slot.settle(Verdict::Flushed))
            .collect();
        let step = if progressed || self.round == Round::Rerun {
            self.round = Round::Running;
            Step::Continue
        } else {
            self.round = Round::Idle;
            Step::Quiescent
        };
        Settlement { wakers, step }
    }

    /// Return the round to `Idle` after its runner vanished mid-round.
    pub(crate) fn abandon_round(&mut self) {
        self.round = Round::Idle;
    }
}
