//! Per-lane ingress ordering tickets.
//!
//! Sequences increase monotonically and `reserve` publishes `Pending` before
//! any matching `Ready`, so the actor pops a lane in sequence order however the
//! decoding tasks interleave; frames of one lane decode in parallel. Dropping an
//! active ticket publishes `Cancel`, so no abandoned sequence can block the
//! actor lane.

use futures::channel::mpsc;

use super::InboundEvent;
use super::InboundLane;
use crate::error::Error;
use crate::error::Result;

/// Command sent to the inbound actor to reserve, deliver, or cancel a sequenced event.
pub(super) enum InboundCommand {
    /// The sequence of an event of `lane` ordered in `category` (`swarm::callback::inbound`'s
    /// per-category barrier), reserved before the event is ready.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    Pending {
        sequence: u64,
        lane: InboundLane,
        category: InboundLane,
    },
    /// A decoded event ready for ordered dispatch in `category`.
    Ready {
        /// Ordering category lane for the ready event.
        category: InboundLane,
        /// Ready inbound event payload.
        event: Box<InboundEvent>,
    },
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    Cancel { sequence: u64, lane: InboundLane },
}

/// Sequenced sender handle for publishing [`InboundCommand`]s to the inbound actor.
pub(super) struct InboundSender {
    /// Underlying unbounded channel to the inbound actor.
    sender: mpsc::UnboundedSender<InboundCommand>,
    /// Next monotonic sequence number to assign.
    next_sequence: u64,
}

impl InboundSender {
    /// Construct a new [`InboundSender`] starting at sequence `0`.
    pub(super) fn new(sender: mpsc::UnboundedSender<InboundCommand>) -> Self {
        Self {
            sender,
            next_sequence: 0,
        }
    }

    /// Assign the next sequence number and enqueue a ready event built by `build_event`.
    pub(super) fn submit_ready(
        &mut self,
        category: InboundLane,
        build_event: impl FnOnce(u64) -> InboundEvent,
    ) -> Result<()> {
        let sequence = self.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(Error::InboundActorInvariantViolation)?;
        let event = build_event(sequence);
        self.sender
            .unbounded_send(InboundCommand::Ready {
                category,
                event: Box::new(event),
            })
            .map_err(|_| Error::InboundMailboxClosed)?;
        self.next_sequence = next_sequence;
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn reserve(
        &mut self,
        lane: InboundLane,
        category: InboundLane,
    ) -> Result<InboundTicket> {
        let sequence = self.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(Error::InboundActorInvariantViolation)?;
        self.sender
            .unbounded_send(InboundCommand::Pending {
                sequence,
                lane,
                category,
            })
            .map_err(|_| Error::InboundMailboxClosed)?;

        self.next_sequence = next_sequence;
        Ok(InboundTicket {
            sender: self.sender.clone(),
            sequence,
            lane,
            active: true,
        })
    }

    /// Return whether the inbound actor's receiving end has been closed.
    pub(super) fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn close_channel(&mut self) {
        self.sender.close_channel();
    }
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(super) struct InboundTicket {
    sender: mpsc::UnboundedSender<InboundCommand>,
    sequence: u64,
    lane: InboundLane,
    active: bool,
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
impl Drop for InboundTicket {
    fn drop(&mut self) {
        if self.active {
            let _ = self.sender.unbounded_send(InboundCommand::Cancel {
                sequence: self.sequence,
                lane: self.lane,
            });
        }
    }
}
