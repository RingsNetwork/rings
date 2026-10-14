//! `Law (Lock order)` of the read-only views (#946).

use std::cell::Cell;

use crate::dht::delivery::RouteStage;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::ecc::SecretKey;
use crate::error::Result;
use crate::storage::MemStorage;

/// A ring at a fresh DID that admitted one peer, and that peer.
fn ring_with_one_peer() -> Result<(PeerRing, Did)> {
    let local: Did = SecretKey::random().address().into();
    let peer: Did = SecretKey::random().address().into();
    let ring = PeerRing::new_with_storage(local, 3, Box::new(MemStorage::new()));
    ring.admit_connected(peer, None)?;
    Ok((ring, peer))
}

/// How one view consulted its link predicate: how often, and how often under the lock.
#[derive(Debug, Default)]
struct Consultations {
    /// Evaluations of the predicate.
    evaluations: Cell<usize>,
    /// Evaluations made while the transition lock was held.
    locked: Cell<usize>,
}

impl Consultations {
    /// A link predicate over `ring` that records every evaluation here and links `peer` only.
    fn predicate<'a>(&'a self, ring: &'a PeerRing, peer: Did) -> impl Fn(Did) -> bool + 'a {
        move |candidate| {
            self.evaluations.set(self.evaluations.get() + 1);
            if ring.topology_transition.try_lock().is_err() {
                self.locked.set(self.locked.get() + 1);
            }
            candidate == peer
        }
    }

    /// Assert the predicate was consulted, and never under the transition lock.
    fn assert_outside_lock(&self, view: &str) {
        assert!(
            self.evaluations.get() > 0,
            "{view} consulted the link predicate"
        );
        assert_eq!(self.locked.get(), 0, "{view} consulted it under the lock");
    }
}

/// Every delivery decision evaluates its link predicate after the transition lock is released.
///
/// The predicate stands for the transport's, which takes the connection lifecycle boundary
/// that admission holds across a topology transition. Under the transition lock it would
/// invert `lifecycle ≺ topology_transition`, and a routed send would deadlock against a
/// concurrent admission. The probe reads the lock with `try_lock`, which fails on the thread
/// that holds it, so the violation is observed deterministically instead of as a hang.
#[test]
fn test_delivery_decisions_evaluate_link_predicate_outside_transition_lock() -> Result<()> {
    let (ring, peer) = ring_with_one_peer()?;

    let delivery_step = Consultations::default();
    ring.delivery_step(
        peer,
        RouteStage::TOWARD,
        delivery_step.predicate(&ring, peer),
    )?;
    delivery_step.assert_outside_lock("delivery_step");

    let reply_via = Consultations::default();
    ring.reply_via(reply_via.predicate(&ring, peer))?;
    reply_via.assert_outside_lock("reply_via");

    let origination = Consultations::default();
    ring.origination(peer, origination.predicate(&ring, peer))?;
    origination.assert_outside_lock("origination");
    Ok(())
}
