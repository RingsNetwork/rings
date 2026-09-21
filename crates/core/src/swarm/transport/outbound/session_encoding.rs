//! The sessions this end has sent one peer inline: the table the outbound worker encodes frames
//! against, shared with the inbound side that marks the peer's confirmations and answers its
//! questions about it; and the budget of link-control sends this end keeps in flight to the
//! peer.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;

use super::OutboundSchedulers;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::LinkControl;
use crate::message::MessagePayload;
use crate::message::WirePayload;
use crate::session::SessionDigest;
use crate::swarm::callback::INBOUND_PEER_CAPACITY;
use crate::swarm::session_link::AnnouncedSessions;

/// Link-control sends this end keeps in flight to one peer at most: two per inbound frame the
/// peer may have in flight here, since each inbound frame causes at most two of them. A send
/// beyond it is refused; the frame is idempotent, and the next frame that misses or teaches
/// the same session repeats it.
pub(crate) const LINK_CONTROL_IN_FLIGHT_CAPACITY: usize = 2 * INBOUND_PEER_CAPACITY;

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
            .ok()
            .map(|_| LinkControlPermit(Arc::clone(&self.0)))
    }
}

/// One link-control send counted in its peer's [`LinkControlBudget`] for as long as it lives.
pub(crate) struct LinkControlPermit(Arc<AtomicUsize>);

impl Drop for LinkControlPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The one handle on a peer's [`AnnouncedSessions`]: every access is one pure step under the
/// lock.
///
/// Lock law: the lock is never held across a suspension point, and a poisoned lock still guards
/// a well-formed table, since no step panics between two writes. Clone law: clones name the
/// same table.
#[derive(Clone)]
pub(super) struct SharedAnnouncedSessions(Arc<Mutex<AnnouncedSessions>>);

impl SharedAnnouncedSessions {
    /// The table of a peer nothing has been sent to.
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(AnnouncedSessions::new())))
    }

    /// The table, for one step.
    fn lock(&self) -> MutexGuard<'_, AnnouncedSessions> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The bytes of `payload` on the link of `generation` at `now_ms`: its session slots are
    /// decided by the table, confirmed sessions by digest and every other session inline.
    pub(super) fn encode(
        &self,
        generation: u64,
        payload: &MessagePayload,
        now_ms: u128,
    ) -> Result<Bytes> {
        let sessions = self.lock().encode(generation, payload, now_ms)?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        super::test_trace::record_encoded_frame(
            (
                crate::message::MessageVerificationExt::signer(payload),
                payload.relay.next_hop,
            ),
            &sessions,
        );
        WirePayload::view(payload, sessions).to_wire()
    }
}

impl OutboundSchedulers {
    /// The announced-session table of `peer`'s link, if this end has a scheduler for it: the
    /// registry lock is released before the table is touched.
    fn announced_of(&self, peer: Did) -> Option<SharedAnnouncedSessions> {
        self.lock_registry()
            .peers
            .get(&peer)
            .map(|handle| handle.state.announced.clone())
    }

    /// The peer confirmed `digest` on the link of `generation`: later frames to it reference
    /// the session. A peer this end has no scheduler for was sent nothing.
    pub(in crate::swarm::transport) fn acknowledge_session(
        &self,
        peer: Did,
        generation: u64,
        digest: SessionDigest,
    ) {
        if let Some(announced) = self.announced_of(peer) {
            announced.lock().acknowledge(generation, digest);
        }
    }

    /// Answer `peer`'s question about `digest` on the link of `generation` at `now_ms`. A peer
    /// this end has no scheduler for was sent nothing, so nothing is known to it.
    pub(in crate::swarm::transport) fn answer_session_request(
        &self,
        peer: Did,
        generation: u64,
        digest: SessionDigest,
        now_ms: u128,
    ) -> LinkControl {
        self.announced_of(peer)
            .map_or(LinkControl::Unknown(digest), |announced| {
                announced.lock().answer(generation, digest, now_ms)
            })
    }

    /// One link-control send to `peer` counted against its budget, refused as
    /// [`Error::LinkControlInFlightCapacity`] when the budget is spent.
    pub(in crate::swarm::transport) fn link_control_permit(
        &self,
        peer: Did,
    ) -> Result<LinkControlPermit> {
        self.handle(peer)?
            .state
            .link_control
            .try_reserve()
            .ok_or(Error::LinkControlInFlightCapacity(peer))
    }
}

#[cfg(test)]
mod test_session_encoding {
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
