//! The crossing-offer decision table (`Law (Crossing offer)`, `Law (Tie-break)`).

use super::crossing_offer_verdict;
use super::CrossingOfferVerdict;
use super::Handshake;
use super::OfferOwner;
use super::OfferPhase;
use crate::dht::Did;

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
    for handshake in [answered, Handshake::answering(), Handshake::Answering {
        reply_sent: true,
    }] {
        for (local, peer) in [(smaller, larger), (larger, smaller)] {
            assert_eq!(
                crossing_offer_verdict(&handshake, local, peer),
                CrossingOfferVerdict::Refuse
            );
        }
    }
}
