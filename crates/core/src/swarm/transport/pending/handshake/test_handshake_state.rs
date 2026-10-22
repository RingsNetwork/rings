//! The crossing-offer decision table (`Law (Crossing offer)`, `Law (Tie-break)`), and the
//! release a yield applies only in the state it was decided on (`Law (Decided state)`).

use super::crossing_offer_verdict;
use super::CrossingOfferVerdict;
use super::Handshake;
use super::OfferOwner;
use super::OfferPhase;
use crate::dht::Did;
use crate::error::Result;
use crate::swarm::transport::pending::ConnectionLifecycleRegistry;
use crate::swarm::transport::pending::LifecycleBounds;

/// The two DIDs in ascending order.
fn ordered() -> (Did, Did) {
    (Did::from(1_u32), Did::from(2_u32))
}

/// An offering attempt of `owner` whose offer was sent under `tx_id`.
fn sent(owner: OfferOwner, tx_id: uuid::Uuid) -> Handshake {
    Handshake::Offering {
        owner,
        phase: OfferPhase::Sent {
            sdp: "offer".to_string(),
            tx_id,
        },
    }
}

/// An offer still being created yields in both DID orders: nothing crossed yet.
#[test]
fn test_creating_offer_yields_whatever_the_did_order() {
    let (smaller, larger) = ordered();
    for owner in [OfferOwner::Overlay, OfferOwner::External] {
        let creating = Handshake::offering(owner);
        assert_eq!(
            crossing_offer_verdict(&creating, smaller, larger),
            CrossingOfferVerdict::Yield
        );
        assert_eq!(
            crossing_offer_verdict(&creating, larger, smaller),
            CrossingOfferVerdict::Yield
        );
    }
}

/// A sent overlay offer prevails at the smaller DID, which sends it back under its own
/// transaction, and yields at the larger DID.
#[test]
fn test_sent_overlay_offer_follows_the_tie_break() {
    let (smaller, larger) = ordered();
    let tx_id = uuid::Uuid::new_v4();
    let offer = sent(OfferOwner::Overlay, tx_id);
    assert_eq!(
        crossing_offer_verdict(&offer, smaller, larger),
        CrossingOfferVerdict::CounterOffer {
            sdp: "offer".to_string(),
            tx_id,
        }
    );
    assert_eq!(
        crossing_offer_verdict(&offer, larger, smaller),
        CrossingOfferVerdict::Yield
    );
}

/// A sent external offer never counter-offers: its owner carries it.
#[test]
fn test_sent_external_offer_never_counter_offers() {
    let (smaller, larger) = ordered();
    let offer = sent(OfferOwner::External, uuid::Uuid::new_v4());
    assert_eq!(
        crossing_offer_verdict(&offer, smaller, larger),
        CrossingOfferVerdict::Refuse
    );
    assert_eq!(
        crossing_offer_verdict(&offer, larger, smaller),
        CrossingOfferVerdict::Yield
    );
}

/// An answered offer and an answering attempt refuse a delayed or duplicate offer in both
/// DID orders, and never counter-offer.
#[test]
fn test_answered_and_answering_attempts_refuse() {
    let (smaller, larger) = ordered();
    let answered = Handshake::Offering {
        owner: OfferOwner::Overlay,
        phase: OfferPhase::Answered,
    };
    for handshake in [answered, Handshake::Answering] {
        for (local, peer) in [(smaller, larger), (larger, smaller)] {
            assert_eq!(
                crossing_offer_verdict(&handshake, local, peer),
                CrossingOfferVerdict::Refuse
            );
        }
    }
}

/// A yield decided while the offer was `Creating` does not release it once it left as `Sent`:
/// the yield is refused and the attempt keeps its slot, while a yield decided on the current
/// state applies and hands the slot to the answering generation in the same transition
/// (`Law (Atomic yield)`), even with the pending bound full.
#[test]
fn test_a_yield_releases_only_the_state_it_was_decided_on() -> Result<()> {
    let (_, peer) = ordered();
    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 1));
    let attempt = registry.reserve_for(peer, 0, Handshake::offering(OfferOwner::Overlay))?;
    let decided = Handshake::offering(OfferOwner::Overlay);

    let tx_id = uuid::Uuid::new_v4();
    assert!(registry.mark_offer_sent(attempt, "offer".to_string(), tx_id));
    assert_eq!(registry.replace_pending_in(attempt, &decided, 1)?, None);
    assert_eq!(registry.pending_attempt(peer), Some(attempt));

    let current = sent(OfferOwner::Overlay, tx_id);
    let answering = registry
        .replace_pending_in(attempt, &current, 1)?
        .expect("a yield decided on the current state applies");
    assert_ne!(answering, attempt);
    assert_eq!(registry.pending_attempt(peer), Some(answering));
    assert_eq!(registry.handshake(answering), Some(&Handshake::Answering));
    assert_eq!(registry.handshake(attempt), None);
    assert!(registry.superseded_by_peer(attempt));
    Ok(())
}
