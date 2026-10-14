//! The signaling state of a pending attempt (#947).
//!
//! A pending generation is either offering or answering, and its offer or answer advances
//! through the phases below. The state lives in the lifecycle registry beside the pending
//! record and is created and dropped by the same transitions, so no reader can observe a
//! pending attempt without its signaling state or a signaling state without its attempt.
//!
//! ```text
//! Offering(o): Creating ──sent(sdp, tx)──▶ Sent(sdp, tx) ──answer──▶ Answered
//! Answering:   reply_sent = false ──reply handed to the carrier──▶ reply_sent = true
//! ```
//!
//! Law (Crossing offer): an offer from `peer` meeting this node's pending attempt for `peer`
//! is decided by [`crossing_offer_verdict`] alone, a pure function of the attempt's signaling
//! state and the two DIDs.

use crate::dht::Did;

/// Who drives an offering attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(crate) enum OfferOwner {
    /// The overlay: the offer travels over the DHT and may be sent back as a counter-offer.
    Overlay,
    /// An external owner (`Swarm::offer_connection`, the HTTP handshake): the offer is carried
    /// out of band by its owner, so it is never sent over the DHT as a counter-offer.
    External,
}

/// How far an offering attempt has got.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(crate) enum OfferPhase {
    /// The offer is being created (ICE gathering); nothing has left this node.
    Creating,
    /// The offer left under transaction `tx_id` and is unanswered.
    Sent {
        /// The offer's SDP as sent, for a counter-offer.
        sdp: String,
        /// The transaction the offer left under; its answer echoes it.
        tx_id: uuid::Uuid,
    },
    /// An answer to the offer is being, or was, applied.
    Answered,
}

/// The signaling state of one pending attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(crate) enum Handshake {
    /// This node offered.
    Offering {
        /// Who drives the offer.
        owner: OfferOwner,
        /// How far the offer has got.
        phase: OfferPhase,
    },
    /// This node answers the peer's offer. Any further offer from the peer is refused while
    /// the answer is in flight (a copy of the answered offer included): the slot is that
    /// handshake's until it is admitted, released, or expires.
    Answering {
        /// Whether the answer was handed to its carrier.
        reply_sent: bool,
    },
}

impl Handshake {
    /// A new offering attempt of `owner`, still creating its offer.
    pub(crate) const fn offering(owner: OfferOwner) -> Self {
        Self::Offering {
            owner,
            phase: OfferPhase::Creating,
        }
    }

    /// A new answering attempt whose answer is not sent yet.
    pub(crate) const fn answering() -> Self {
        Self::Answering { reply_sent: false }
    }
}

/// What an offer from a peer does to this node's pending attempt for that peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CrossingOfferVerdict {
    /// This node abandons its attempt and answers the offer.
    Yield,
    /// This node keeps its unanswered overlay offer, sent under `tx_id`, and sends it back.
    CounterOffer {
        /// The offer's SDP.
        sdp: String,
        /// The transaction the offer left under.
        tx_id: uuid::Uuid,
    },
    /// The offer is refused: this node's attempt owns the slot.
    Refuse,
}

/// Decide an offer from `peer` meeting this node's (`local`) pending attempt in `handshake`.
///
/// ```text
/// Offering(_, Creating)          ──▶ Yield            nothing crossed yet: no tie
/// Offering(o, Sent(s, t)), l < p ──▶ CounterOffer(s, t)   if o = Overlay
///                                ──▶ Refuse               if o = External
/// Offering(_, Sent(_)),    l > p ──▶ Yield            the smaller DID's offer prevails
/// Offering(_, Answered)          ──▶ Refuse           an answer is being applied
/// Answering(_)                   ──▶ Refuse           the slot answers the peer's own offer
/// ```
///
/// Law (Tie-break): of two crossing unanswered offers exactly one side yields, the larger DID,
/// and the other sends its own offer back unless an external owner carries it; so whenever
/// either overlay offer is delivered the edge is established.
pub(crate) fn crossing_offer_verdict(
    handshake: &Handshake,
    local: Did,
    peer: Did,
) -> CrossingOfferVerdict {
    match handshake {
        Handshake::Offering {
            phase: OfferPhase::Creating,
            ..
        } => CrossingOfferVerdict::Yield,
        Handshake::Offering {
            phase: OfferPhase::Sent { .. },
            ..
        } if local > peer => CrossingOfferVerdict::Yield,
        Handshake::Offering {
            owner: OfferOwner::Overlay,
            phase: OfferPhase::Sent { sdp, tx_id },
        } => CrossingOfferVerdict::CounterOffer {
            sdp: sdp.clone(),
            tx_id: *tx_id,
        },
        Handshake::Offering {
            owner: OfferOwner::External,
            phase: OfferPhase::Sent { .. },
        }
        | Handshake::Offering {
            phase: OfferPhase::Answered,
            ..
        }
        | Handshake::Answering { .. } => CrossingOfferVerdict::Refuse,
    }
}

#[cfg(test)]
mod test_handshake_state;
