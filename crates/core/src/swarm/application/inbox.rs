//! The application inbox: the one queue from core to the application.
//!
//! ```text
//!   ApplicationOutlet (core)                     ApplicationInbox (application)
//!     announce(event)         ──┐
//!     deliver(message)        ──┼──▶ Queue (one order) ──next()──▶ the application
//!     try_reserve(m) ▸ fill() ──┘        take ▸ consumed(m) fires
//! ```
//!
//! Laws, over the queue `(claim, items, weight)`:
//! - **Never waits on the application.** Every operation of the outlet returns at once. A live
//!   message that does not fit is dropped, counted (`dropped`) and logged at `warn`: application
//!   delivery is lossy by design, as a best-effort network is, and no lane ever waits for the
//!   application to read. A protocol that needs a reliable stream detects loss end to end
//!   itself (#939).
//! - **One order.** Messages and peer facts share one queue, in the order core produced them,
//!   so a peer fact never overtakes an earlier message from the same peer. Facts describe
//!   overlay membership, not a gate on messages: a message's identity is its origin, not the
//!   link that carried it, so a verified message from a peer may follow that peer's
//!   retirement, as relayed mail from any non-neighbour does. Only a frame whose carrier was
//!   retired before dispatch is withheld, at the inbound gate.
//! - **Bounded messages.** `Σ weight(messages) + reserved ≤ APPLICATION_INBOX_WEIGHT`, where a
//!   message weighs its own size and the length of its data, except that an inbox holding no
//!   message and no reservation admits one of any weight. A message is weighed once, when it
//!   is admitted; its weight leaves the queue with it.
//! - **Bounded facts.** An admission is withdrawn together with its retirement while unread
//!   (`announce(PeerRetired p)` cancels a queued `PeerAdmitted p`), so per peer the queue holds
//!   at most `[PeerRetired]` followed by `[PeerAdmitted]`: `|facts| ≤ 2 · |peers|` over the peers
//!   admitted when last read or now. Facts are never dropped.
//! - **Unclaimed.** Before the application claims the inbox, live messages are discarded (no
//!   application can be owed them) and facts are kept, so a late claimer reads the peers already
//!   admitted. No room is reserved then, so stored mail stays stored.
//! - **Released.** Once the inbox is dropped, everything is discarded and it cannot be claimed
//!   again.
//! - **Consumption.** A message filled into a reserved room carries a consumption signal, fired
//!   when the application takes it from the queue, so stored mail is retired only once taken; a
//!   signal whose message is discarded is dropped unfired. Taking is where core's custody ends:
//!   what the application does with the message afterwards is the application's.

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

/// Why the queue refuses a message's weight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    /// No application reads the queue: the message is discarded.
    Unread,
    /// The message does not fit: a live one is dropped and counted.
    Full,
}

/// One queued item: the delivery, the weight it holds, and the signal its taking fires.
struct Queued {
    delivery: ApplicationDelivery,
    /// The weight admitted with a message; zero for a fact.
    weight: usize,
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

impl Queue {
    /// Whether `weight` more bytes fit: always into an inbox holding none.
    fn fits(&self, weight: usize) -> bool {
        self.weight == 0 || self.weight.saturating_add(weight) <= APPLICATION_INBOX_WEIGHT
    }

    /// Admit `weight` bytes for one message, if an application reads the queue and it fits.
    fn admit(&mut self, weight: usize) -> std::result::Result<(), Refusal> {
        if self.claim != Claim::Claimed {
            return Err(Refusal::Unread);
        }
        if !self.fits(weight) {
            return Err(Refusal::Full);
        }
        self.weight = self.weight.saturating_add(weight);
        Ok(())
    }

    /// Queue `message` under the `weight` [`Self::admit`] admitted for it. An inbox released
    /// since then discards it, together with its signal: its weight left with the release.
    fn push(
        &mut self,
        message: ApplicationMessage,
        weight: usize,
        consumed: Option<oneshot::Sender<()>>,
    ) {
        if self.claim == Claim::Claimed {
            self.items.push_back(Queued {
                delivery: ApplicationDelivery::Message(message),
                weight,
                consumed,
            });
        }
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
            weight: 0,
            consumed: None,
        });
    }

    /// The next item, releasing its weight and firing its consumption signal.
    fn take(&mut self) -> Option<ApplicationDelivery> {
        let Queued {
            delivery,
            weight,
            consumed,
        } = self.items.pop_front()?;
        self.weight = self.weight.saturating_sub(weight);
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

    /// Queue a live `message`; discard it when no application reads the inbox, or drop, count
    /// and log it when it does not fit, as the module laws say.
    pub(crate) fn deliver(&self, message: ApplicationMessage) {
        let weight = message.weight();
        let mut queue = self.shared.lock();
        match queue.admit(weight) {
            Ok(()) => {
                queue.push(message, weight, None);
                drop(queue);
                self.shared.ready.notify(1);
            }
            Err(Refusal::Unread) => {}
            Err(Refusal::Full) => {
                queue.dropped = queue.dropped.saturating_add(1);
                let dropped = queue.dropped;
                drop(queue);
                tracing::warn!(
                    tx_id = %message.tx_id,
                    from = %message.from,
                    dropped,
                    "application inbox full: live message dropped"
                );
            }
        }
    }

    /// Room holding `message`, its weight reserved until it is filled or dropped, if an
    /// application reads the inbox and the message fits now. Stored mail is offered only through
    /// a reservation, so mail that finds no room, or no reader, stays where it is stored.
    pub(crate) fn try_reserve(&self, message: ApplicationMessage) -> Option<InboxRoom> {
        let weight = message.weight();
        self.shared.lock().admit(weight).ok()?;
        Some(InboxRoom {
            held: HeldWeight {
                shared: self.shared.clone(),
                weight,
            },
            message,
        })
    }
}

/// Weight reserved in the inbox, returned when dropped unless a message took it over.
struct HeldWeight {
    shared: Arc<Shared>,
    /// The weight still held; zero once a queued message holds it instead.
    weight: usize,
}

impl Drop for HeldWeight {
    fn drop(&mut self) {
        if self.weight > 0 {
            let mut queue = self.shared.lock();
            queue.weight = queue.weight.saturating_sub(self.weight);
        }
    }
}

/// Room for one message, reserved in the inbox together with the message it holds: filling it
/// never waits, and the weight queued is the weight reserved, by construction.
pub(crate) struct InboxRoom {
    held: HeldWeight,
    message: ApplicationMessage,
}

impl InboxRoom {
    /// The message this room holds.
    pub(crate) fn message(&self) -> &ApplicationMessage {
        &self.message
    }

    /// Queue the held message. The returned receiver resolves once the application takes it,
    /// and is cancelled if the inbox discards it first, as a release since the reservation does.
    pub(crate) fn fill(self) -> oneshot::Receiver<()> {
        let Self { mut held, message } = self;
        let (consumed, taken) = oneshot::channel();
        let weight = std::mem::take(&mut held.weight);
        held.shared.lock().push(message, weight, Some(consumed));
        held.shared.ready.notify(1);
        taken
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

    /// The counter of live messages core dropped because the inbox was full: application
    /// delivery is lossy by design, and this is how an application observes the loss.
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
