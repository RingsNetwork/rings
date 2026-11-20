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
            handshake: "offer".to_string(),
            tx_id,
        },
    }
}

/// The two carriers of a crossing offer.
const CARRIERS: [OfferOwner; 2] = [OfferOwner::Overlay, OfferOwner::External];

/// An offer still being created yields in both DID orders, whatever carries the crossing offer:
/// nothing crossed yet.
#[test]
fn test_creating_offer_yields_whatever_the_did_order() {
    let (smaller, larger) = ordered();
    for owner in CARRIERS {
        let creating = Handshake::offering(owner);
        for carrier in CARRIERS {
            assert_eq!(
                crossing_offer_verdict(&creating, carrier, smaller, larger),
                CrossingOfferVerdict::Yield
            );
            assert_eq!(
                crossing_offer_verdict(&creating, carrier, larger, smaller),
                CrossingOfferVerdict::Yield
            );
        }
    }
}

/// A sent overlay offer meeting an overlay offer prevails at the smaller DID, which sends it
/// back under its own transaction, and yields at the larger DID.
#[test]
fn test_sent_overlay_offer_follows_the_tie_break() {
    let (smaller, larger) = ordered();
    let tx_id = uuid::Uuid::new_v4();
    let offer = sent(OfferOwner::Overlay, tx_id);
    assert_eq!(
        crossing_offer_verdict(&offer, OfferOwner::Overlay, smaller, larger),
        CrossingOfferVerdict::CounterOffer {
            handshake: "offer".to_string(),
            tx_id,
        }
    );
    assert_eq!(
        crossing_offer_verdict(&offer, OfferOwner::Overlay, larger, smaller),
        CrossingOfferVerdict::Yield
    );
}

/// A sent external offer meeting an out-of-band offer never counter-offers, since its owner
/// carries it: it is refused at the smaller DID and yields at the larger.
#[test]
fn test_sent_external_offer_never_counter_offers() {
    let (smaller, larger) = ordered();
    let offer = sent(OfferOwner::External, uuid::Uuid::new_v4());
    assert_eq!(
        crossing_offer_verdict(&offer, OfferOwner::External, smaller, larger),
        CrossingOfferVerdict::Refuse
    );
    assert_eq!(
        crossing_offer_verdict(&offer, OfferOwner::External, larger, smaller),
        CrossingOfferVerdict::Yield
    );
}

/// Of a sent overlay offer and a sent out-of-band offer, the out-of-band one prevails in both
/// DID orders: the overlay side yields and the out-of-band side refuses, so exactly one side
/// yields and no counter-offer reaches an HTTP join (`Law (Tie-break)`).
#[test]
fn test_out_of_band_offer_prevails_over_an_overlay_offer() {
    let (smaller, larger) = ordered();
    let overlay = sent(OfferOwner::Overlay, uuid::Uuid::new_v4());
    let external = sent(OfferOwner::External, uuid::Uuid::new_v4());
    for (local, peer) in [(smaller, larger), (larger, smaller)] {
        assert_eq!(
            crossing_offer_verdict(&overlay, OfferOwner::External, local, peer),
            CrossingOfferVerdict::Yield
        );
        assert_eq!(
            crossing_offer_verdict(&external, OfferOwner::Overlay, peer, local),
            CrossingOfferVerdict::Refuse
        );
    }
}

/// Of any two crossing sent offers exactly one side yields: the verdict at one end, on its
/// offer meeting the other's, yields iff the verdict at the other end does not.
#[test]
fn test_exactly_one_of_two_sent_offers_yields() {
    let (smaller, larger) = ordered();
    for local_owner in CARRIERS {
        for peer_owner in CARRIERS {
            let local_offer = sent(local_owner, uuid::Uuid::new_v4());
            let peer_offer = sent(peer_owner, uuid::Uuid::new_v4());
            for (local, peer) in [(smaller, larger), (larger, smaller)] {
                let here = crossing_offer_verdict(&local_offer, peer_owner, local, peer);
                let there = crossing_offer_verdict(&peer_offer, local_owner, peer, local);
                assert_ne!(
                    here == CrossingOfferVerdict::Yield,
                    there == CrossingOfferVerdict::Yield,
                    "{local_owner:?} at {local:?} against {peer_owner:?} at {peer:?}"
                );
            }
        }
    }
}

/// An answered offer and an answering attempt refuse a delayed or duplicate offer in both
/// DID orders, whatever carries it, and never counter-offer.
#[test]
fn test_answered_and_answering_attempts_refuse() {
    let (smaller, larger) = ordered();
    let answered = Handshake::Offering {
        owner: OfferOwner::Overlay,
        phase: OfferPhase::Answered,
    };
    for handshake in [answered, Handshake::Answering] {
        for carrier in CARRIERS {
            for (local, peer) in [(smaller, larger), (larger, smaller)] {
                assert_eq!(
                    crossing_offer_verdict(&handshake, carrier, local, peer),
                    CrossingOfferVerdict::Refuse
                );
            }
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

/// Formal poset & absorbing-state model check over the complete 7-state `Handshake` space:
///
/// 1. `rank(Creating) = 0 < rank(Sent) = 1 < rank(Answered) = rank(Answering) = 2`.
/// 2. Every state of maximal rank (`rank == 2`: `Offering(Overlay, Answered)`,
///    `Offering(External, Answered)`, and `Answering`) is an absorbing terminal state under
///    `crossing_offer_verdict`: it returns `CrossingOfferVerdict::Refuse` for every carrier
///    (`Overlay` and `External`) and every DID order (`smaller < larger` and `larger > smaller`),
///    so an in-flight `Answering` or `Answered` generation can never be self-preempted by a
///    duplicate or overlapping offer.
/// 3. Whenever `crossing_offer_verdict(h, ..) == Yield`, applying `replace_pending_in` transitions
///    the registry slot to `Handshake::Answering` (`rank == 2 > rank(h)`), ensuring any yield loop
///    terminates in at most one replacement step.
#[test]
fn test_handshake_signaling_poset_rank_and_absorbing_answering_invariants() -> Result<()> {
    fn rank(handshake: &Handshake) -> u8 {
        match handshake {
            Handshake::Offering {
                phase: OfferPhase::Creating,
                ..
            } => 0,
            Handshake::Offering {
                phase: OfferPhase::Sent { .. },
                ..
            } => 1,
            Handshake::Offering {
                phase: OfferPhase::Answered,
                ..
            }
            | Handshake::Answering => 2,
        }
    }

    let (smaller, larger) = ordered();
    let tx_id = uuid::Uuid::new_v4();
    let all_states = [
        Handshake::offering(OfferOwner::Overlay),
        Handshake::offering(OfferOwner::External),
        sent(OfferOwner::Overlay, tx_id),
        sent(OfferOwner::External, tx_id),
        Handshake::Offering {
            owner: OfferOwner::Overlay,
            phase: OfferPhase::Answered,
        },
        Handshake::Offering {
            owner: OfferOwner::External,
            phase: OfferPhase::Answered,
        },
        Handshake::Answering,
    ];

    for state in &all_states {
        let r = rank(state);
        for carrier in CARRIERS {
            for (local, peer) in [(smaller, larger), (larger, smaller)] {
                let verdict = crossing_offer_verdict(state, carrier, local, peer);
                if r == 2 {
                    assert_eq!(
                        verdict,
                        CrossingOfferVerdict::Refuse,
                        "absorbing rank-2 state {state:?} must refuse carrier {carrier:?} ({local:?} vs {peer:?})"
                    );
                }
                if verdict == CrossingOfferVerdict::Yield {
                    assert!(
                        r < 2,
                        "only rank < 2 states may yield, but {state:?} yielded"
                    );
                    let mut registry = ConnectionLifecycleRegistry::new(LifecycleBounds::new(1, 1));
                    let initial = registry.reserve_for(peer, 0, state.clone())?;
                    let replaced = registry
                        .replace_pending_in(initial, state, 1)?
                        .expect("yield on current state must succeed");
                    let next_state = registry
                        .handshake(replaced)
                        .expect("replacement generation must have a signaling state");
                    assert_eq!(next_state, &Handshake::Answering);
                    assert!(
                        rank(next_state) > r,
                        "yield must strictly increase signaling rank ({r} -> {})",
                        rank(next_state)
                    );
                    // Once in Answering, any subsequent offer (Overlay or External) is refused.
                    for next_carrier in CARRIERS {
                        assert_eq!(
                            crossing_offer_verdict(next_state, next_carrier, local, peer),
                            CrossingOfferVerdict::Refuse
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
