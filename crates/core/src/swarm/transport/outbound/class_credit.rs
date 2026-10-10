//! The transport credit of each class's lane, as the outbound worker holds it: reserved apart
//! from the timed send, keyed by connection generation, and gating which classes may admit a
//! frame.

use std::future::Future;
use std::pin::Pin;

use futures::FutureExt;
use rings_transport::core::transport::LaneCreditReservation;

use super::model;
use super::OutboundWorker;
use super::TransferClass;
use super::CLASSES;
use crate::error::Result;
use crate::swarm::transport::PendingConnectionAttempt;
use crate::utils::get_epoch_ms_i64;

/// The transport credit of one class's lane, as the worker holds it.
///
/// The worker admits a frame only of a class whose lane holds a credit, and acquires credits
/// for the other runnable classes while it serves them: a lane waiting for its receiver (the
/// transport's backpressure) never holds the worker from another lane, and the send it admits
/// is timed only for the transport's acceptance, never for the receiver's consumption. A
/// detached payload's first-frame deadline does include its credit wait, apart from its sender
/// (see `SwarmTransport::send_payload_enqueued`). A credit wait never retires the generation: a
/// slow receiver is backpressure, and liveness judges the peer.
///
/// Law (generation). A credit, and the verdict that none can be had, belong to the connection
/// generation they were reserved on: the worker outlives generations, and a credit of one
/// generation settles only that generation's window. A slot whose generation is not that of the
/// class's next transfer is reset, returning its credit, and the class reserves anew.
///
/// Law (wait identity). A pending reservation settles only the slot that still awaits it: once
/// the slot is reset, by a generation change or otherwise, the reservation's outcome is
/// dropped, returning its credit, whatever the slot holds by then.
pub(super) enum ClassCredit {
    /// No credit is held or awaited.
    Idle,
    /// The reservation `wait` on the generation `attempt` is pending in
    /// [`OutboundWorker::credit_waits`], since `since_ms`.
    Awaiting(PendingConnectionAttempt, i64, CreditWaitId),
    /// A credit is held for the class's next frame on the generation `attempt`.
    Held(LaneCreditReservation, PendingConnectionAttempt),
    /// The lane of the generation `attempt` cannot grant credit (its connection is gone): the
    /// next frame on it is sent without one, and fails as its connection does.
    Unavailable(PendingConnectionAttempt),
}

impl ClassCredit {
    /// The generation the slot's credit, verdict or pending reservation belongs to.
    pub(super) const fn generation(&self) -> Option<PendingConnectionAttempt> {
        match self {
            Self::Held(_, attempt) | Self::Unavailable(attempt) | Self::Awaiting(attempt, ..) => {
                Some(*attempt)
            }
            Self::Idle => None,
        }
    }

    /// The generation the slot's credit or verdict belongs to, if it holds one: what may
    /// admit a frame. A pending reservation admits none.
    pub(super) const fn attempt(&self) -> Option<PendingConnectionAttempt> {
        match self {
            Self::Held(_, attempt) | Self::Unavailable(attempt) => Some(*attempt),
            Self::Idle | Self::Awaiting(..) => None,
        }
    }

    /// Since when the slot has waited for its lane's credit, if it is waiting.
    const fn awaiting_since_ms(&self) -> Option<i64> {
        match self {
            Self::Awaiting(_, since_ms, _) => Some(*since_ms),
            Self::Idle | Self::Held(..) | Self::Unavailable(_) => None,
        }
    }
}

/// The identity of one pending credit reservation, unique within its worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CreditWaitId(u64);

/// The outcome of one pending credit reservation, for the class and wait it was made for.
pub(super) type CreditWaitOutput = (
    TransferClass,
    CreditWaitId,
    PendingConnectionAttempt,
    Result<LaneCreditReservation>,
);

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) type CreditWaitFuture = Pin<Box<dyn Future<Output = CreditWaitOutput> + Send>>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) type CreditWaitFuture = Pin<Box<dyn Future<Output = CreditWaitOutput>>>;

impl OutboundWorker {
    /// Reserve a transport credit for every class with a runnable transfer and none held or
    /// awaited.
    ///
    /// A credit available at once is held now; otherwise its reservation joins
    /// [`Self::credit_waits`], and the class stays gated until it resolves.
    pub(super) fn acquire_credits(&mut self) {
        for class in CLASSES {
            let index = class.index();
            let Some(next) = self.ready.next_of(class) else {
                continue;
            };
            let admitted = next.scheduled.transfer.admitted.clone();
            let attempt = admitted.attempt();
            if let Some(slot) = self.credits.get_mut(index) {
                if slot.generation().is_some_and(|held| held != attempt) {
                    // Another generation's credit settles only its own window: return it, and
                    // abandon a reservation still pending on it (its outcome is then ignored).
                    *slot = ClassCredit::Idle;
                }
            }
            if !matches!(self.credits.get(index), Some(ClassCredit::Idle)) {
                continue;
            }
            // The wait owns the lane's credit state only, so a long wait pins no connection.
            let credit = admitted
                .connection()
                .reserve_send_credit(model::channel_lane(class));
            let id = self.next_credit_wait();
            let mut wait: CreditWaitFuture =
                Box::pin(async move { (class, id, attempt, credit.await) });
            match (&mut wait).now_or_never() {
                Some((_, _, attempt, credit)) => self.hold_credit(class, attempt, credit),
                None => {
                    if let Some(slot) = self.credits.get_mut(index) {
                        *slot = ClassCredit::Awaiting(attempt, get_epoch_ms_i64(), id);
                    }
                    self.credit_waits.push(wait);
                }
            }
        }
        self.publish_credit_stall();
    }

    /// Publish since when this worker has waited for a lane's credit (its oldest pending
    /// reservation), for liveness to probe a peer that withholds credit.
    fn publish_credit_stall(&self) {
        let since_ms = self
            .credits
            .iter()
            .filter_map(ClassCredit::awaiting_since_ms)
            .min();
        self.credit_stall.set(since_ms);
    }

    /// A fresh credit wait identity.
    pub(super) fn next_credit_wait(&mut self) -> CreditWaitId {
        let id = CreditWaitId(self.next_credit_wait);
        self.next_credit_wait = self.next_credit_wait.wrapping_add(1);
        id
    }

    /// Record the outcome of `class`'s pending credit reservation `wait` on the generation
    /// `attempt`, if the slot still awaits it; otherwise (the wait identity law) the outcome is
    /// dropped, returning its credit.
    pub(super) fn settle_credit(
        &mut self,
        class: TransferClass,
        wait: CreditWaitId,
        attempt: PendingConnectionAttempt,
        credit: Result<LaneCreditReservation>,
    ) {
        if matches!(
            self.credits.get(class.index()),
            Some(ClassCredit::Awaiting(_, _, awaited)) if *awaited == wait
        ) {
            self.hold_credit(class, attempt, credit);
        }
    }

    /// Hold the outcome of a reservation for `class` on the generation `attempt` in its slot.
    fn hold_credit(
        &mut self,
        class: TransferClass,
        attempt: PendingConnectionAttempt,
        credit: Result<LaneCreditReservation>,
    ) {
        let settled = match credit {
            Ok(credit) => ClassCredit::Held(credit, attempt),
            Err(error) => {
                tracing::debug!(class = ?class, ?error, "outbound lane cannot reserve credit");
                ClassCredit::Unavailable(attempt)
            }
        };
        if let Some(slot) = self.credits.get_mut(class.index()) {
            *slot = settled;
        }
        self.publish_credit_stall();
    }

    /// Whether `class` may admit its next frame: its slot holds a credit, or the verdict that
    /// none can be had (the frame then fails with its connection), of the generation of the
    /// class's next transfer.
    pub(super) fn grants_a_frame(&self, class: TransferClass) -> bool {
        let next = self
            .ready
            .next_of(class)
            .map(|next| next.scheduled.transfer.admitted.attempt());
        let held = self
            .credits
            .get(class.index())
            .and_then(ClassCredit::attempt);
        next.is_some() && held == next
    }

    /// Take the credit held for `class`'s next frame, leaving the class to reserve anew.
    ///
    /// Pre: [`Self::grants_a_frame`]`(class)` admitted the frame, so the slot is of the
    /// generation of the class's next transfer: the generation law is enforced by that gate,
    /// which a slot of another generation never passes.
    pub(super) fn take_credit(&mut self, class: TransferClass) -> Option<LaneCreditReservation> {
        let slot = self.credits.get_mut(class.index())?;
        match std::mem::replace(slot, ClassCredit::Idle) {
            ClassCredit::Held(credit, _) => Some(credit),
            ClassCredit::Idle | ClassCredit::Awaiting(..) | ClassCredit::Unavailable(_) => None,
        }
    }
}
