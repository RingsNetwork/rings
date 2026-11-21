//! The signaling state of a pending attempt (#947, #1291).
//!
//! A pending generation is either offering (initiator) or answering (responder) across a
//! `0..MAX_HANDSHAKE_STEPS` transport handshake. The state lives in the lifecycle registry beside
//! the pending record and is created and dropped by the same transitions, so no reader can observe
//! a pending attempt without its signaling state or a signaling state without its attempt.
//!
//! ```text
//! Offering(o): Creating ──sent(0)──▶ Sent(0, final, bytes, tx) ──claim──▶ ClaimedReply(2k, sid) ──sent──▶ Sent(2k, ..) / Answered
//! Answering:   Answering(None) ◀──claim/await──▶ Answering(Some(2k, sid))
//! ```
//!
//! Law (Crossing offer): an initial offer (`step_index == 0`) from `peer` meeting this node's
//! pending attempt for `peer` is decided by [`crossing_offer_verdict`] alone, a pure function of
//! the attempt's signaling state, the offer's carrier and the two DIDs.

use bytes::Bytes;

use crate::dht::Did;

/// Who drives an offering attempt, and so who carries its offer. A crossing offer is carried by
/// the overlay when it arrives over the DHT, and by an external owner when it is handed in out of
/// band (`Swarm::answer_offer`, the HTTP handshake's `answerOffer`).
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
    /// Step `0` is being created; nothing has left this node.
    Creating,
    /// Outbound initiator step `step_index` (`0` for the initial offer, even `2, 4, ...` for
    /// multi-round continuations) left under transaction `tx_id`. When `final_step` is `false`, it
    /// awaits odd reply `step_index + 1`; when `step_index == 0 && final_step == true` (a 1-step
    /// announce), it expects no reply while still participating in `Law (Tie-break)` against a
    /// crossing initial offer from the peer.
    Sent {
        /// Zero-based even step index (`0, 2, 4, ...`) sent by the initiator.
        step_index: u8,
        /// Whether this sent step was final (`true` only for 1-step `step_index == 0` offers;
        /// final continuation steps `>= 2` move directly to [`Self::Answered`]).
        final_step: bool,
        /// The step's binary handshake payload (retained for counter-offering when `step_index == 0`).
        handshake: Bytes,
        /// The transaction the step left under; its reply echoes it.
        tx_id: uuid::Uuid,
        /// Optional 128-bit session correlation ID for `>= 3`-step handshakes.
        session_id: Option<[u8; 16]>,
    },
    /// Atomically claimed an inbound non-final odd `ConnectNodeReport` and is now computing and
    /// sending even continuation step `next_step_index` (`2, 4, ...`) under `session_id`. Prevents
    /// duplicate reports under the same transaction from racing into `handshake_step`.
    ClaimedReply {
        /// Next even outbound step index (`2, 4, ...`) to be produced and marked sent.
        next_step_index: u8,
        /// Session correlation ID established for this multi-step handshake.
        session_id: [u8; 16],
    },
    /// All handshake signaling steps have completed (`Done` or `SendFinal` at `step_index >= 2`);
    /// awaiting channel open.
    Answered,
}

/// The signaling state of one pending attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Hash))]
pub(crate) enum Handshake {
    /// This node initiated the handshake.
    Offering {
        /// Who drives the offer.
        owner: OfferOwner,
        /// How far the offer has got.
        phase: OfferPhase,
    },
    /// This node answers the peer's offer. Any crossing initial offer (`step_index == 0`) from the
    /// peer is refused while the answer is in flight; when a multi-round responder step (`Send`)
    /// awaits even continuation step `step_index` under `session_id`, `awaiting_step` holds
    /// `Some((step_index, session_id))`.
    Answering {
        /// Expected next even initiator continuation `(step_index, session_id)`, if any.
        awaiting_step: Option<(u8, [u8; 16])>,
    },
}

impl Handshake {
    /// The signaling state a test reserves a generation in by default: an external offer still
    /// being created.
    #[cfg(test)]
    pub(crate) const TEST_DEFAULT: Self = Self::offering(OfferOwner::External);

    /// A new offering attempt of `owner`, still creating its initial step.
    pub(crate) const fn offering(owner: OfferOwner) -> Self {
        Self::Offering {
            owner,
            phase: OfferPhase::Creating,
        }
    }

    /// A new responder attempt answering an initial step (`step_index == 0`).
    pub(crate) const fn answering() -> Self {
        Self::Answering {
            awaiting_step: None,
        }
    }
}

/// What an initial offer (`step_index == 0`) from a peer does to this node's pending attempt for
/// that peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CrossingOfferVerdict {
    /// This node abandons its attempt and answers the offer; the answering generation replaces
    /// the attempt in one transition (`Law (Atomic yield)`).
    Yield,
    /// This node keeps its unanswered initial overlay offer, sent under `tx_id`, and sends it back.
    CounterOffer {
        /// Whether the initial offer was a 1-step final step (`true`) or expects a reply (`false`).
        final_step: bool,
        /// The initial offer's binary handshake payload.
        handshake: Bytes,
        /// Optional session correlation ID sent with step `0`.
        session_id: Option<[u8; 16]>,
        /// The transaction the offer left under.
        tx_id: uuid::Uuid,
    },
    /// The offer is refused: this node's attempt owns the slot.
    Refuse,
}

/// Decide an initial offer (`step_index == 0`) from `peer`, carried by `carrier`, meeting this
/// node's (`local`) pending attempt in `handshake`.
///
/// ```text
/// Offering(_, Creating)                        ──▶ Yield     nothing crossed yet: no tie
/// Offering(Overlay, Sent(0, ..)), c = External ──▶ Yield     the out-of-band offer prevails
/// Offering(External, Sent(0, ..)), c = Overlay ──▶ Refuse    this node's out-of-band offer prevails
/// Offering(o, Sent(0, h, t, s)), c = o, l < p  ──▶ CounterOffer(h, s, t) if o = Overlay
///                                              ──▶ Refuse                if o = External
/// Offering(o, Sent(0, ..)),      c = o, l > p  ──▶ Yield     the smaller DID's offer prevails
/// Offering(_, Sent(2k>0, ..))                  ──▶ Refuse    responder already answered step 0
/// Offering(_, Answered)                        ──▶ Refuse    signaling steps completed
/// Answering { .. }                             ──▶ Refuse    the slot answers the peer's own offer
/// ```
///
/// Law (Tie-break): of two crossing unanswered initial offers (`step_index == 0`) exactly one side
/// yields. Once step `0` has been answered (either moving to `Sent { step_index >= 2 }` in a
/// multi-round handshake or `Answered`), the attempt is absorbing (`Refuse`), preventing a delayed
/// copy of step `0` from preempting an already-matched multi-step exchange.
pub(crate) fn crossing_offer_verdict(
    handshake: &Handshake,
    carrier: OfferOwner,
    local: Did,
    peer: Did,
) -> CrossingOfferVerdict {
    match handshake {
        Handshake::Offering {
            phase: OfferPhase::Creating,
            ..
        } => CrossingOfferVerdict::Yield,
        Handshake::Offering {
            owner: OfferOwner::Overlay,
            phase: OfferPhase::Sent { step_index: 0, .. },
        } if carrier == OfferOwner::External => CrossingOfferVerdict::Yield,
        Handshake::Offering {
            owner: OfferOwner::External,
            phase: OfferPhase::Sent { step_index: 0, .. },
        } if carrier == OfferOwner::Overlay => CrossingOfferVerdict::Refuse,
        Handshake::Offering {
            phase: OfferPhase::Sent { step_index: 0, .. },
            ..
        } if local > peer => CrossingOfferVerdict::Yield,
        Handshake::Offering {
            owner: OfferOwner::Overlay,
            phase:
                OfferPhase::Sent {
                    step_index: 0,
                    final_step,
                    handshake,
                    tx_id,
                    session_id,
                },
        } => CrossingOfferVerdict::CounterOffer {
            final_step: *final_step,
            handshake: handshake.clone(),
            session_id: *session_id,
            tx_id: *tx_id,
        },
        Handshake::Offering {
            owner: OfferOwner::External,
            phase: OfferPhase::Sent { step_index: 0, .. },
        }
        | Handshake::Offering {
            phase: OfferPhase::Sent { .. } | OfferPhase::ClaimedReply { .. } | OfferPhase::Answered,
            ..
        }
        | Handshake::Answering { .. } => CrossingOfferVerdict::Refuse,
    }
}

#[cfg(test)]
mod test_handshake_state;
