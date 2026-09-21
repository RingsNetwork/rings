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

pub(super) enum InboundCommand {
    Pending { sequence: u64, lane: InboundLane },
    Ready(Box<InboundEvent>),
    Cancel { sequence: u64, lane: InboundLane },
}

pub(super) struct InboundSender {
    sender: mpsc::UnboundedSender<InboundCommand>,
    next_sequence: u64,
}

impl InboundSender {
    pub(super) fn new(sender: mpsc::UnboundedSender<InboundCommand>) -> Self {
        Self {
            sender,
            next_sequence: 0,
        }
    }

    pub(super) fn reserve(&mut self, lane: InboundLane) -> Result<InboundTicket> {
        let sequence = self.next_sequence;
        let next_sequence = sequence
            .checked_add(1)
            .ok_or(Error::InboundActorInvariantViolation)?;
        self.sender
            .unbounded_send(InboundCommand::Pending { sequence, lane })
            .map_err(|_| Error::InboundMailboxClosed)?;

        self.next_sequence = next_sequence;
        Ok(InboundTicket {
            sender: self.sender.clone(),
            sequence,
            lane,
            active: true,
        })
    }

    pub(super) fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn close_channel(&mut self) {
        self.sender.close_channel();
    }
}

pub(super) struct InboundTicket {
    sender: mpsc::UnboundedSender<InboundCommand>,
    sequence: u64,
    lane: InboundLane,
    active: bool,
}

impl InboundTicket {
    pub(super) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(super) fn commit(mut self, event: InboundEvent) -> Result<()> {
        self.sender
            .unbounded_send(InboundCommand::Ready(Box::new(event)))
            .map_err(|_| Error::InboundMailboxClosed)?;
        self.active = false;
        Ok(())
    }
}

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
