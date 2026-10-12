//! The application inbox: the one queue from core to the application.
//!
//! ```text
//!   ApplicationOutlet (core)                     ApplicationInbox (application)
//!     announce(event)         ──┐
//!     deliver(message)        ──┼──▶ Queue (one order) ──next()──▶ the application
//!     try_reserve ▸ fill(m, ⊥|t)┘        take ▸ consumed(t) fires
//! ```
//!
//! Laws, over the queue `(claim, items, weight)`:
//! - **Never waits on the application.** Every operation of the outlet returns at once. A live
//!   message that does not fit is dropped and counted (`dropped`), as a best-effort network
//!   drops it; no lane ever waits for the application to read (#913 R10 H1, H2, M4).
//! - **One order.** Messages and peer facts share one queue, in the order core produced them,
//!   so a peer fact never overtakes an earlier message from the same peer. Facts describe
//!   overlay membership, not a gate on messages: a message's identity is its origin, not the
//!   link that carried it, so a verified message from a peer may follow that peer's
//!   retirement, as relayed mail from any non-neighbour does. Only a frame whose carrier was
//!   retired before dispatch is withheld, at the inbound gate.
//! - **Bounded messages.** `Σ weight(messages) + reserved ≤ APPLICATION_INBOX_WEIGHT`, where a
//!   message weighs its own size and its heap bytes, except that an inbox holding no message
//!   and no reservation admits one of any weight.
//! - **Bounded facts.** An admission is withdrawn together with its retirement while unread
//!   (`announce(PeerRetired p)` cancels a queued `PeerAdmitted p`), so per peer the queue holds
//!   at most `[PeerRetired]` followed by `[PeerAdmitted]`: `|facts| ≤ 2 · |peers|` over the peers
//!   admitted when last read or now. Facts are never dropped.
//! - **Unclaimed.** Before the application claims the inbox, live messages are discarded (no
//!   application can be owed them) and facts are kept, so a late claimer reads the peers already
//!   admitted. No room is reserved then, so stored mail stays stored.
//! - **Released.** Once the inbox is dropped, everything is discarded and it cannot be claimed
//!   again.
//! - **Consumption.** A message queued through a reservation may carry a consumption signal,
//!   fired when the application takes it, so stored mail is retired only once read (#913 R10
//!   H3); a signal whose message is discarded is dropped unfired.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use event_listener::Event;
use futures::channel::oneshot;

use super::ApplicationDelivery;
use super::ApplicationMessage;
use super::SwarmEvent;
use crate::error::Error;
use crate::error::Result;

/// The bytes of application data the inbox retains; a live message beyond it is dropped.
pub const APPLICATION_INBOX_WEIGHT: usize = 64 * 1024 * 1024;

/// Who reads the queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Claim {
    /// No application yet: messages are discarded, facts kept.
    Unclaimed,
    /// An [`ApplicationInbox`] reads the queue.
    Claimed,
    /// The inbox was dropped: everything is discarded.
    Released,
}

/// One queued item, with the signal its consumption fires, if any.
struct Queued {
    delivery: ApplicationDelivery,
    consumed: Option<oneshot::Sender<()>>,
}

/// The state both ends share under one lock.
struct Queue {
    claim: Claim,
    /// Whether core dropped its outlet: no item will ever be added.
    closed: bool,
    items: VecDeque<Queued>,
    /// `Σ weight(messages)` plus the weight every live [`InboxRoom`] holds.
    weight: usize,
    /// Live messages dropped because they did not fit.
    dropped: u64,
}

/// What [`ApplicationOutlet::deliver`] did with a live message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Delivered {
    /// Queued for the application.
    Queued,
    /// Discarded: no application reads the inbox.
    Discarded,
    /// Dropped and counted: the inbox is full.
    Dropped,
}

impl Queue {
    /// Whether `weight` more bytes fit: always into an inbox holding none.
    fn fits(&self, weight: usize) -> bool {
        self.weight == 0 || self.weight.saturating_add(weight) <= APPLICATION_INBOX_WEIGHT
    }

    /// Queue `message` if an application reads the queue and it fits.
    fn offer(&mut self, message: ApplicationMessage) -> Delivered {
        if self.claim != Claim::Claimed {
            return Delivered::Discarded;
        }
        let weight = message.weight();
        if !self.fits(weight) {
            self.dropped = self.dropped.saturating_add(1);
            return Delivered::Dropped;
        }
        self.weight = self.weight.saturating_add(weight);
        self.items.push_back(Queued {
            delivery: ApplicationDelivery::Message(message),
            consumed: None,
        });
        Delivered::Queued
    }

    /// Record `event`, withdrawing an unread admission its retirement ends.
    fn record(&mut self, event: SwarmEvent) {
        if self.claim == Claim::Released {
            return;
        }
        if let SwarmEvent::PeerRetired { peer } = event {
            let unread_admission = self
                .items
                .iter()
                .enumerate()
                .rev()
                .find(|(_, queued)| match &queued.delivery {
                    ApplicationDelivery::Event(fact) => fact.peer() == peer,
                    ApplicationDelivery::Message(_) => false,
                })
                .filter(|(_, last)| {
                    matches!(
                        last.delivery,
                        ApplicationDelivery::Event(SwarmEvent::PeerAdmitted { .. })
                    )
                })
                .map(|(at, _)| at);
            if let Some(at) = unread_admission {
                self.items.remove(at);
                return;
            }
        }
        self.items.push_back(Queued {
            delivery: ApplicationDelivery::Event(event),
            consumed: None,
        });
    }

    /// The next item, firing its consumption signal.
    fn take(&mut self) -> Option<ApplicationDelivery> {
        let Queued { delivery, consumed } = self.items.pop_front()?;
        if let ApplicationDelivery::Message(message) = &delivery {
            self.weight = self.weight.saturating_sub(message.weight());
        }
        if let Some(consumed) = consumed {
            let _ = consumed.send(());
        }
        Some(delivery)
    }

    /// Discard everything and refuse every later item.
    fn release(&mut self) {
        self.claim = Claim::Released;
        self.items.clear();
        self.weight = 0;
    }
}

/// The queue and its wake-up.
struct Shared {
    queue: Mutex<Queue>,
    /// Notified when an item is added or the outlet closes.
    ready: Event,
}

impl Shared {
    /// The queue; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Core's end of the inbox. Held by the transport; never waits on the application.
pub(crate) struct ApplicationOutlet {
    shared: Arc<Shared>,
}

impl ApplicationOutlet {
    /// An unclaimed, empty inbox.
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    claim: Claim::Unclaimed,
                    closed: false,
                    items: VecDeque::new(),
                    weight: 0,
                    dropped: 0,
                }),
                ready: Event::new(),
            }),
        }
    }

    /// Hand the reading end to the application, once.
    pub(crate) fn claim(&self) -> Result<ApplicationInbox> {
        let mut queue = self.shared.lock();
        if queue.claim != Claim::Unclaimed {
            return Err(Error::ApplicationInboxClaimed);
        }
        queue.claim = Claim::Claimed;
        Ok(ApplicationInbox {
            shared: self.shared.clone(),
        })
    }

    /// Record a peer fact.
    pub(crate) fn announce(&self, event: SwarmEvent) {
        self.shared.lock().record(event);
        self.shared.ready.notify(1);
    }

    /// Queue a live `message`, or discard or drop it as the module laws say.
    pub(crate) fn deliver(&self, message: ApplicationMessage) -> Delivered {
        let delivered = self.shared.lock().offer(message);
        if delivered == Delivered::Queued {
            self.shared.ready.notify(1);
        }
        delivered
    }

    /// Room for `message`, held until filled or dropped, if an application reads the inbox and
    /// the message fits now. Stored mail is offered only through a reservation, so mail that
    /// finds no room, or no reader, stays where it is stored.
    pub(crate) fn try_reserve(&self, message: &ApplicationMessage) -> Option<InboxRoom> {
        let weight = message.weight();
        let mut queue = self.shared.lock();
        if queue.claim != Claim::Claimed || !queue.fits(weight) {
            return None;
        }
        queue.weight = queue.weight.saturating_add(weight);
        Some(InboxRoom {
            shared: self.shared.clone(),
            held: Some(weight),
        })
    }
}

/// Room for one message, reserved in the inbox: filling it never waits.
pub(crate) struct InboxRoom {
    shared: Arc<Shared>,
    /// The weight held, until the room is filled.
    held: Option<usize>,
}

impl InboxRoom {
    /// Queue `message` into this room, with `consumed` to fire when the application takes it.
    /// Pre: `message` is the one the room was reserved for, so its weight is the weight held.
    /// Returns whether it was queued: an inbox released meanwhile discards it.
    pub(crate) fn fill(
        mut self,
        message: ApplicationMessage,
        consumed: oneshot::Sender<()>,
    ) -> bool {
        // `held` is `Some` from the reservation until here; taking it tells `Drop` the room was
        // filled.
        let held = self.held.take().unwrap_or_default();
        let mut queue = self.shared.lock();
        if queue.claim != Claim::Claimed {
            return false;
        }
        queue.weight = queue
            .weight
            .saturating_sub(held)
            .saturating_add(message.weight());
        queue.items.push_back(Queued {
            delivery: ApplicationDelivery::Message(message),
            consumed: Some(consumed),
        });
        drop(queue);
        self.shared.ready.notify(1);
        true
    }
}

impl Drop for InboxRoom {
    fn drop(&mut self) {
        if let Some(held) = self.held {
            let mut queue = self.shared.lock();
            queue.weight = queue.weight.saturating_sub(held);
        }
    }
}

impl Drop for ApplicationOutlet {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.ready.notify(usize::MAX);
    }
}

/// The application's end of the inbox: what core hands it, in order, at the pace it reads.
///
/// Claimed once through `Swarm::application_inbox`. Dropping it releases the inbox: from then on
/// core discards live application data and leaves stored mail stored.
pub struct ApplicationInbox {
    shared: Arc<Shared>,
}

impl ApplicationInbox {
    /// The next item, waiting for one; `None` once the swarm is gone and the queue is drained.
    /// Cancel-safe: an item is taken only when it is returned.
    pub async fn next(&self) -> Option<ApplicationDelivery> {
        loop {
            let listener = {
                let mut queue = self.shared.lock();
                if let Some(item) = queue.take() {
                    return Some(item);
                }
                if queue.closed {
                    return None;
                }
                self.shared.ready.listen()
            };
            listener.await;
        }
    }

    /// The next item if one is queued. Never waits.
    pub fn try_next(&self) -> Option<ApplicationDelivery> {
        self.shared.lock().take()
    }

    /// How many live messages were dropped because the inbox was full.
    pub fn dropped(&self) -> u64 {
        self.shared.lock().dropped
    }
}

impl Drop for ApplicationInbox {
    fn drop(&mut self) {
        self.shared.lock().release();
    }
}

#[cfg(test)]
mod tests;
