//! The application inbox: the one queue from core to the application.
//!
//! ```text
//!   ApplicationOutlet (core)                       ApplicationInbox (application)
//!     announce(event)    ──never waits──┐
//!     deliver(message)   ──waits room───┼──▶ Queue ──next()──▶ events first, then messages
//!     try_reserve(message) ──None──────┘  InboxRoom::fill(message) never waits
//! ```
//!
//! Laws, over the queue `(claim, events, messages)`:
//! - **Never waits on the application.** `announce` and `try_deliver` return at once;
//!   `deliver` waits only for room, so only the caller's own lane waits.
//! - **Bounded messages.** `Σ weight(messages) + reserved ≤ APPLICATION_INBOX_WEIGHT`, except
//!   that an empty queue admits one message of any weight, so every message eventually fits. A
//!   reservation holds its weight until it is filled or dropped.
//! - **Bounded events.** An admission is withdrawn together with its retirement while unread
//!   (`announce(PeerRetired p)` cancels a queued `PeerAdmitted p`), so per peer the queue holds
//!   at most `[PeerRetired]` followed by `[PeerAdmitted]`: `|events| ≤ 2 · |peers|` over the
//!   peers admitted when last read or now.
//! - **Unclaimed.** Before the application claims the inbox, messages are discarded (no
//!   application can be owed them) and events are kept, so a late claimer reads the peers already
//!   admitted.
//! - **Released.** Once the inbox is dropped, everything is discarded and it cannot be claimed
//!   again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use event_listener::Event;

use super::ApplicationDelivery;
use super::ApplicationMessage;
use super::SwarmEvent;
use crate::error::Error;
use crate::error::Result;

/// The bytes of application data the inbox retains before `deliver` waits for room.
pub const APPLICATION_INBOX_WEIGHT: usize = 64 * 1024 * 1024;

/// Who reads the queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Claim {
    /// No application yet: messages are discarded, events kept.
    Unclaimed,
    /// An [`ApplicationInbox`] reads the queue.
    Claimed,
    /// The inbox was dropped: everything is discarded.
    Released,
}

/// The state both ends share under one lock.
struct Queue {
    claim: Claim,
    /// Whether core dropped its outlet: no item will ever be added.
    closed: bool,
    events: VecDeque<SwarmEvent>,
    messages: VecDeque<ApplicationMessage>,
    /// `Σ weight(messages)` plus the weight every live [`InboxRoom`] holds.
    weight: usize,
}

/// What an offer of one message did.
enum Offer {
    /// Queued, or discarded because no application reads the queue.
    Taken,
    /// No room: the message is handed back.
    Full(ApplicationMessage),
}

impl Queue {
    /// Whether `weight` more bytes fit: always into an empty queue.
    fn fits(&self, weight: usize) -> bool {
        self.weight == 0 || self.weight.saturating_add(weight) <= APPLICATION_INBOX_WEIGHT
    }

    /// Offer `message`: taken iff no application reads the queue or it fits.
    fn offer(&mut self, message: ApplicationMessage) -> Offer {
        if self.claim != Claim::Claimed {
            return Offer::Taken;
        }
        let weight = message.weight();
        if !self.fits(weight) {
            return Offer::Full(message);
        }
        self.weight = self.weight.saturating_add(weight);
        self.messages.push_back(message);
        Offer::Taken
    }

    /// Record `event`, withdrawing an unread admission its retirement ends.
    fn record(&mut self, event: SwarmEvent) {
        if self.claim == Claim::Released {
            return;
        }
        if let SwarmEvent::PeerRetired { peer } = event {
            let unread_admission = self
                .events
                .iter()
                .enumerate()
                .rev()
                .find(|(_, queued)| queued.peer() == peer)
                .filter(|(_, last)| matches!(last, SwarmEvent::PeerAdmitted { .. }))
                .map(|(at, _)| at);
            if let Some(at) = unread_admission {
                self.events.remove(at);
                return;
            }
        }
        self.events.push_back(event);
    }

    /// The next item: events first, so a message never delays a peer fact.
    fn take(&mut self) -> Option<ApplicationDelivery> {
        if let Some(event) = self.events.pop_front() {
            return Some(ApplicationDelivery::Event(event));
        }
        let message = self.messages.pop_front()?;
        self.weight = self.weight.saturating_sub(message.weight());
        Some(ApplicationDelivery::Message(message))
    }

    /// Discard everything and refuse every later item.
    fn release(&mut self) {
        self.claim = Claim::Released;
        self.events.clear();
        self.messages.clear();
        self.weight = 0;
    }
}

/// The queue and its two wake-ups.
struct Shared {
    queue: Mutex<Queue>,
    /// Notified when an item is added or the outlet closes.
    ready: Event,
    /// Notified when room is made, or when no application reads the queue any more.
    room: Event,
}

impl Shared {
    /// The queue; every critical section leaves it consistent, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Core's end of the inbox. Held by the transport; never awaits the application.
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
                    events: VecDeque::new(),
                    messages: VecDeque::new(),
                    weight: 0,
                }),
                ready: Event::new(),
                room: Event::new(),
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

    /// Record a peer fact. Never waits.
    pub(crate) fn announce(&self, event: SwarmEvent) {
        self.shared.lock().record(event);
        self.shared.ready.notify(1);
    }

    /// Queue `message`, waiting only for room. Cancel-safe: a cancelled wait queued nothing.
    pub(crate) async fn deliver(&self, mut message: ApplicationMessage) {
        loop {
            let listener = {
                let mut queue = self.shared.lock();
                match queue.offer(message) {
                    Offer::Taken => break,
                    Offer::Full(back) => {
                        message = back;
                        self.shared.room.listen()
                    }
                }
            };
            listener.await;
        }
        self.shared.ready.notify(1);
    }

    /// Room for `message` if it fits now, held until filled or dropped. Never waits.
    ///
    /// Whether the message is owed to an application is decided here: room reserved before the
    /// inbox is claimed discards its message, as an unclaimed inbox discards every message.
    pub(crate) fn try_reserve(&self, message: &ApplicationMessage) -> Option<InboxRoom> {
        let weight = message.weight();
        let mut queue = self.shared.lock();
        let held = match queue.claim {
            Claim::Claimed if queue.fits(weight) => Some(weight),
            Claim::Claimed => return None,
            Claim::Unclaimed | Claim::Released => None,
        };
        if let Some(held) = held {
            queue.weight = queue.weight.saturating_add(held);
        }
        Some(InboxRoom {
            shared: self.shared.clone(),
            held,
        })
    }
}

/// Room for one message, reserved in the inbox: filling it never waits.
pub(crate) struct InboxRoom {
    shared: Arc<Shared>,
    /// The weight held, or `None` when no application read the queue at reservation.
    held: Option<usize>,
}

impl InboxRoom {
    /// Queue `message` into this room. Pre: `message` is the one the room was reserved for, so
    /// its weight is the weight held.
    pub(crate) fn fill(mut self, message: ApplicationMessage) {
        let Some(held) = self.held.take() else {
            return;
        };
        let mut queue = self.shared.lock();
        if queue.claim == Claim::Claimed {
            queue.weight = queue
                .weight
                .saturating_sub(held)
                .saturating_add(message.weight());
            queue.messages.push_back(message);
        }
        drop(queue);
        self.shared.ready.notify(1);
    }
}

impl Drop for InboxRoom {
    fn drop(&mut self) {
        let Some(held) = self.held else {
            return;
        };
        let mut queue = self.shared.lock();
        queue.weight = queue.weight.saturating_sub(held);
        drop(queue);
        self.shared.room.notify(usize::MAX);
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
/// core discards application data instead of waiting for room.
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
                    drop(queue);
                    self.shared.room.notify(usize::MAX);
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
        let item = self.shared.lock().take()?;
        self.shared.room.notify(usize::MAX);
        Some(item)
    }
}

impl Drop for ApplicationInbox {
    fn drop(&mut self) {
        self.shared.lock().release();
        self.shared.room.notify(usize::MAX);
    }
}

#[cfg(test)]
mod tests;
