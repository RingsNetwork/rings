//! Per-lane ingress ordering tickets.
//!
//! Sequences increase monotonically. `reserve` publishes `Pending` before any
//! matching `Ready`, and each ticket releases its admission turn exactly once.
//! The normal path releases immediately after core capacity admission so later
//! same-lane frames can decode in parallel; `commit` and `Drop` are idempotent
//! fallbacks. Dropping an active ticket also publishes `Cancel`, so no abandoned
//! sequence can block the actor lane.

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::FutureExt;

use super::InboundEvent;
use super::InboundLane;
use crate::error::Error;
use crate::error::Result;

type AdmissionTurn = Shared<oneshot::Receiver<()>>;

pub(super) enum InboundCommand {
    Pending { sequence: u64, lane: InboundLane },
    Ready(Box<InboundEvent>),
    Cancel { sequence: u64, lane: InboundLane },
}

pub(super) struct InboundSender {
    sender: mpsc::UnboundedSender<InboundCommand>,
    next_sequence: u64,
    lane_tails: [Option<AdmissionTurn>; super::INBOUND_LANE_COUNT],
}

impl InboundSender {
    pub(super) fn new(sender: mpsc::UnboundedSender<InboundCommand>) -> Self {
        Self {
            sender,
            next_sequence: 0,
            lane_tails: std::array::from_fn(|_| None),
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

        let predecessor = self.lane_tails.get(lane.index()).cloned().flatten();
        let (release, released) = oneshot::channel();
        if let Some(tail) = self.lane_tails.get_mut(lane.index()) {
            *tail = Some(released.shared());
        }
        self.next_sequence = next_sequence;
        Ok(InboundTicket {
            sender: self.sender.clone(),
            sequence,
            lane,
            predecessor,
            release: Some(release),
            active: true,
        })
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
    predecessor: Option<AdmissionTurn>,
    release: Option<oneshot::Sender<()>>,
    active: bool,
}

impl InboundTicket {
    pub(super) const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(super) async fn wait_for_admission_turn(&mut self) {
        if let Some(predecessor) = self.predecessor.take() {
            let _ = predecessor.await;
        }
    }

    pub(super) fn release_admission_turn(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }

    pub(super) fn commit(mut self, event: InboundEvent) -> Result<()> {
        self.release_admission_turn();
        self.sender
            .unbounded_send(InboundCommand::Ready(Box::new(event)))
            .map_err(|_| Error::InboundMailboxClosed)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for InboundTicket {
    fn drop(&mut self) {
        self.release_admission_turn();
        if self.active {
            let _ = self.sender.unbounded_send(InboundCommand::Cancel {
                sequence: self.sequence,
                lane: self.lane,
            });
        }
    }
}
