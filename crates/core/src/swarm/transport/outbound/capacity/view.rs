//! Readings of outbound capacity for a rerouting wait (`swarm::transport::rerouting`): the
//! state its wake guard reads, and the epochs it listens to.
//!
//! ```text
//! PeerStamp    : once, after a refusal was published ─▶ PeerProgress   (ChannelDrain)
//! CapacityView : once per wait iteration             ─▶ Room, listeners (CapacityRelease)
//! ```
//!
//! Both are single readings of the peer's capacity handle, so what a wait listens to and what
//! its guard reads come from the same capacity (`Law (Wake)` of `Epoch`).

use std::sync::Arc;
use std::sync::Weak;

use event_listener::EventListener;

use super::GlobalTransferCapacity;
use super::TransferCapacity;
use super::TransferDemand;
use crate::dht::Did;
use crate::swarm::transport::outbound::OutboundSchedulers;

/// One reading of a peer's capacity handle and the global capacity, from which a wait
/// iteration takes both its `Room` listeners and its `Room` observation.
///
/// Law: `room_listeners` registers on exactly the epochs whose events can change `has_room`
/// of this view, so a listen before the observation loses no wake-up. A capacity created after
/// the reading is not in the view: `has_room` reads the peer as unheld, and a retry it wakes
/// may be refused by the new capacity, a race and not a lost wake-up.
///
/// The view holds the peer capacity strongly; a wait drops it before it awaits, so a waiting
/// placement never keeps a capacity alive.
pub(in crate::swarm::transport) struct CapacityView<'a> {
    /// The global capacity every transfer also holds.
    global: &'a GlobalTransferCapacity,
    /// The peer the view reads.
    peer: Did,
    /// The peer's capacity at the reading, if one existed.
    capacity: Option<Arc<TransferCapacity>>,
}

impl CapacityView<'_> {
    /// `Room(peer, demand)` on this reading (see `TransferCapacity::has_room`), registering
    /// nothing.
    pub(in crate::swarm::transport) fn has_room(&self, demand: TransferDemand) -> bool {
        self.capacity.as_ref().map_or_else(
            || TransferCapacity::has_room_unheld(self.global, self.peer, demand),
            |capacity| capacity.has_room(self.peer, demand),
        )
    }

    /// Register for every event after which `has_room` of this view may change: global and
    /// peer releases, and departures from both queues.
    pub(in crate::swarm::transport) fn room_listeners(&self) -> Vec<EventListener> {
        self.global
            .room_listeners()
            .into_iter()
            .chain(
                self.capacity
                    .iter()
                    .flat_map(|capacity| capacity.room_listeners()),
            )
            .collect()
    }
}

/// A reading of one peer's channel progress, taken after a refused send published its refusal,
/// so after the refused send released whatever it held.
///
/// `ahead` is the number of the peer's transfers holding capacity at the reading: the
/// transfers ahead of a retry on the peer's link. New arrivals do not raise it, so a flowing
/// link drains it after finitely many ends of transfers, and once no transfer arrives the
/// channel is drained of every transfer that was ahead when [`PeerProgress::drained`] holds.
/// `drained` and `ahead` come from one locked reading of the peer state, so no transfer is
/// both ahead and already drained (Inv of `PeerCapacityState::drained`).
///
/// `Drained` counts ends, not identities: under load, transfers admitted after the stamp may
/// end before those ahead and satisfy it early, and the retry may then be refused again. That
/// costs one deferral within the budget and never L1, since nothing is admitted once the
/// environment stops. The exact predicate, "no transfer admitted at the stamp still holds
/// capacity", needs the peer's set of admitted sequence numbers in the reservation state,
/// which is `Copy` and copied by every admission check (`reserved`); an ordered set there
/// would put a set update on every admission and release to remove a budget-bounded cost.
///
/// The capacity is held weakly: a dead capacity means every permit of the peer was released,
/// and a live `Weak` pins the allocation, so a recreated capacity is never mistaken for the
/// stamped one.
pub(in crate::swarm::transport) struct PeerStamp {
    /// The peer's capacity when stamped, if one existed.
    capacity: Weak<TransferCapacity>,
    /// Its drained count when stamped.
    drained: u64,
    /// Its transfers holding capacity when stamped.
    ahead: u64,
}

/// What the peer's capacity shows against a [`PeerStamp`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(in crate::swarm::transport) struct PeerProgress {
    /// `Drained(peer)`: at least `ahead` transfers of the peer that reached its link ended
    /// since the stamp.
    pub(in crate::swarm::transport) drained: bool,
    /// `Idle(peer)`: no transfer of the peer holds capacity.
    pub(in crate::swarm::transport) idle: bool,
}

impl PeerStamp {
    /// The peer's progress now against this stamp; a released capacity is drained and idle.
    ///
    /// `drained − stamped ≥ ahead` counts ends of transfers that reached the link, and
    /// `Idle` covers the transfers ahead that ended without reaching it.
    pub(in crate::swarm::transport) fn progress(&self) -> PeerProgress {
        self.capacity.upgrade().map_or(
            PeerProgress {
                drained: true,
                idle: true,
            },
            |capacity| {
                let reading = ChannelReading::of(&capacity);
                PeerProgress {
                    drained: reading.drained.saturating_sub(self.drained) >= self.ahead,
                    idle: reading.admitted == 0,
                }
            },
        )
    }

    /// Register for every event after which [`PeerStamp::progress`] may change, while the
    /// capacity lives: its progress (`Drained`), and its releases (`Idle`, which a transfer
    /// ending before it reached the link reaches without progress).
    pub(in crate::swarm::transport) fn listen(&self) -> Vec<EventListener> {
        self.capacity
            .upgrade()
            .map(|capacity| vec![capacity.progressed.listen(), capacity.releases.listen()])
            .unwrap_or_default()
    }
}

impl OutboundSchedulers {
    /// The capacity `peer` holds now, if any.
    fn peer_capacity(&self, peer: Did) -> Option<Arc<TransferCapacity>> {
        self.lock_registry()
            .capacities
            .get(&peer)
            .and_then(Weak::upgrade)
    }

    /// Read `peer`'s capacity now (see [`CapacityView`]).
    pub(in crate::swarm::transport) fn capacity_view(&self, peer: Did) -> CapacityView<'_> {
        CapacityView {
            global: &self.global_capacity,
            peer,
            capacity: self.peer_capacity(peer),
        }
    }

    /// Stamp `peer`'s channel progress now (see [`PeerStamp`]).
    pub(in crate::swarm::transport) fn peer_stamp(&self, peer: Did) -> PeerStamp {
        let capacity = self.peer_capacity(peer);
        let reading = capacity.as_ref().map_or(ChannelReading::IDLE, |capacity| {
            ChannelReading::of(capacity)
        });
        PeerStamp {
            capacity: capacity.as_ref().map_or_else(Weak::new, Arc::downgrade),
            drained: reading.drained,
            ahead: u64::try_from(reading.admitted).unwrap_or(u64::MAX),
        }
    }
}

/// One locked reading of a peer's channel counts.
struct ChannelReading {
    /// `PeerCapacityState::drained`.
    drained: u64,
    /// Transfers of the peer holding capacity.
    admitted: usize,
}

impl ChannelReading {
    /// The reading of a peer that holds no capacity.
    const IDLE: Self = Self {
        drained: 0,
        admitted: 0,
    };

    /// Read `capacity`'s counts in one critical section.
    fn of(capacity: &TransferCapacity) -> Self {
        let state = capacity
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            drained: state.drained,
            admitted: state.capacity.admitted_count(),
        }
    }
}
