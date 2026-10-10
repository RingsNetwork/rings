//! The state of one peer's link that the sending end keeps across its outbound workers: the
//! announced-delegation table and the budget of link-control sends in flight. A worker is
//! replaced under an unchanged connection generation without losing either; a newer
//! generation empties the table by itself on its first frame.

use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY;

use super::session_encoding::SharedAnnouncedDelegations;
use super::OutboundSchedulers;
use crate::dht::Did;

/// Link-control sends this end keeps in flight to one peer at most: two per frame the peer may
/// have in flight at this end's transport, since each inbound frame causes at most two of
/// them. A send beyond it is refused; the frame is idempotent, and the next frame that misses
/// or teaches the same session repeats it.
pub(crate) const LINK_CONTROL_IN_FLIGHT_CAPACITY: usize = 2 * INBOUND_PEER_FRAME_CAPACITY;

/// One peer's link state as the sending end keeps it. Clone law: clones name the same link.
#[derive(Clone)]
pub(super) struct PeerLinkState {
    /// The sessions sent inline and which of them the peer confirmed.
    pub(super) announced: SharedAnnouncedDelegations,
    /// The link-control sends in flight to the peer.
    pub(super) control_budget: LinkControlBudget,
    /// Since when the worker has waited for the peer's credit.
    pub(super) credit_stall: CreditStall,
}

impl PeerLinkState {
    /// The link state of a peer nothing has been sent to.
    pub(super) fn new() -> Self {
        Self {
            announced: SharedAnnouncedDelegations::new(),
            control_budget: LinkControlBudget::new(),
            credit_stall: CreditStall::new(),
        }
    }

    /// Whether `other` names the same link: the same tables, not equal ones.
    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) fn is_same_link(&self, other: &Self) -> bool {
        self.announced.is_same_table(&other.announced)
            && Arc::ptr_eq(&self.control_budget.0, &other.control_budget.0)
    }
}

/// Since when the outbound worker of one peer has waited for a lane's credit: the start of its
/// oldest pending reservation, or none. Liveness reads it to probe a peer that withholds credit
/// however recently it sent this end anything. Clone law: clones name the same wait.
#[derive(Clone)]
pub(super) struct CreditStall(Arc<AtomicI64>);

impl CreditStall {
    /// The stored value meaning no reservation is pending.
    const NONE: i64 = i64::MAX;

    /// The stall of a worker that waits for nothing.
    fn new() -> Self {
        Self(Arc::new(AtomicI64::new(Self::NONE)))
    }

    /// Record that the oldest pending reservation began at `since_ms`, or that none is pending.
    pub(super) fn set(&self, since_ms: Option<i64>) {
        self.0
            .store(since_ms.unwrap_or(Self::NONE), Ordering::Release);
    }

    /// Since when a reservation has been pending, if one is.
    fn since_ms(&self) -> Option<i64> {
        Some(self.0.load(Ordering::Acquire)).filter(|since_ms| *since_ms != Self::NONE)
    }
}

/// The count of link-control sends in flight to one peer, bounded by
/// [`LINK_CONTROL_IN_FLIGHT_CAPACITY`]. Clone law: clones count the same sends.
#[derive(Clone)]
pub(super) struct LinkControlBudget(Arc<AtomicUsize>);

impl LinkControlBudget {
    /// The budget of a peer nothing is in flight to.
    pub(super) fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// One more send in flight, if the budget allows it; the permit returns it on drop.
    pub(super) fn try_reserve(&self) -> Option<LinkControlPermit> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |in_flight| {
                (in_flight < LINK_CONTROL_IN_FLIGHT_CAPACITY).then(|| in_flight + 1)
            })
            .ok()?;
        // Test builds: a reserved permit is a state change that quiescence probes read, as a
        // released one is.
        #[cfg(test)]
        crate::tests::activity::record_activity();
        Some(LinkControlPermit(Arc::clone(&self.0)))
    }
}

/// One link-control send counted in its peer's [`LinkControlBudget`] for as long as it lives.
pub(in crate::swarm::transport) struct LinkControlPermit(Arc<AtomicUsize>);

impl Drop for LinkControlPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
        // Test builds: a released permit is a state change that quiescence probes read.
        #[cfg(test)]
        crate::tests::activity::record_activity();
    }
}

impl OutboundSchedulers {
    /// The link state of `peer`, if this end has a worker for it; a lookup, never a spawn.
    /// The registry lock is released before the state is touched.
    pub(super) fn link_of(&self, peer: Did) -> Option<PeerLinkState> {
        self.lock_registry()
            .peers
            .get(&peer)
            .map(|handle| handle.state.link.clone())
    }

    /// Since when this end has waited for `peer`'s credit, if it has a worker for `peer` that
    /// is waiting.
    pub(in crate::swarm::transport) fn credit_stalled_since_ms(&self, peer: Did) -> Option<i64> {
        self.link_of(peer)
            .and_then(|link| link.credit_stall.since_ms())
    }

    /// Record that this end has waited for `peer`'s credit since `since_ms`, as its worker
    /// would, for tests that cannot let a wall-clock interval pass.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(in crate::swarm::transport) fn force_credit_stall_for_test(
        &self,
        peer: Did,
        since_ms: i64,
    ) {
        if let Some(link) = self.link_of(peer) {
            link.credit_stall.set(Some(since_ms));
        }
    }

    /// The link-control sends in flight to `peer`, read without reserving.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(in crate::swarm::transport) fn link_control_in_flight_for_test(&self, peer: Did) -> usize {
        self.link_of(peer)
            .map_or(0, |link| link.control_budget.0.load(Ordering::Acquire))
    }

    /// One link-control send to `peer` counted against its budget: `None` when this end has
    /// no worker for `peer`, `Some(None)` when the budget is spent.
    pub(in crate::swarm::transport) fn link_control_permit(
        &self,
        peer: Did,
    ) -> Option<Option<LinkControlPermit>> {
        self.link_of(peer)
            .map(|link| link.control_budget.try_reserve())
    }
}

#[cfg(test)]
mod test_link_state {
    use super::LinkControlBudget;
    use super::LINK_CONTROL_IN_FLIGHT_CAPACITY;

    /// Law (bound): a peer's budget admits exactly `LINK_CONTROL_IN_FLIGHT_CAPACITY` sends in
    /// flight, refuses the next, and admits again once one ends.
    #[test]
    fn test_link_control_budget_refuses_beyond_capacity_and_recovers_when_a_send_ends() {
        let budget = LinkControlBudget::new();
        let mut permits = (0..LINK_CONTROL_IN_FLIGHT_CAPACITY)
            .map(|_| budget.try_reserve().expect("within capacity"))
            .collect::<Vec<_>>();
        assert!(budget.try_reserve().is_none());
        drop(permits.pop());
        let _readmitted = budget.try_reserve().expect("one send ended");
        assert!(budget.try_reserve().is_none());
    }
}
