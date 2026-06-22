#![warn(missing_docs)]
//! Per-peer renegotiation signaling, as a pure state machine.
//!
//! Adding a media track to a *live* connection changes its SDP, which requires a fresh
//! offer/answer round-trip (renegotiation) over the rings message layer. Doing that naively — "add
//! the track, then send an offer" — is unsafe: two `add_media_track` calls, or both peers offering
//! at once (*glare*), can put multiple offers in flight on one connection with no way to tell a
//! stale answer from the current one. A late answer would then be applied to whatever signaling
//! state the connection happens to be in.
//!
//! This module is the **functional core** that prevents that. [`Negotiator::step`] is a pure
//! function `(state, event) ↦ (state', effect)` — it performs no I/O and touches no WebRTC. The
//! effectful shell ([`crate::swarm::transport`] / [`crate::swarm`]) holds one [`Negotiator`] per
//! peer behind a lock, feeds it events, and carries out the [`NegotiationEffect`] it returns
//! (create/accept SDP, send a message). Because each step is serialized by the per-peer lock, only
//! one local renegotiation is ever outstanding, every offer carries a monotonic
//! [`generation`](NegotiationEffect::SendOffer) id, and an answer whose generation does not match
//! the outstanding offer is dropped.
//!
//! ```text
//!   step : Negotiation × Event ↦ Negotiation × Effect
//!
//!   Idle            , LocalRenegotiate        ↦ AwaitingAnswer(g) , SendOffer(g)     -- g fresh
//!   AwaitingAnswer  , LocalRenegotiate        ↦ AwaitingAnswer    , Busy             -- serialize
//!   Idle            , RemoteOffer(g, _)       ↦ Idle              , SendAnswer(g)
//!   AwaitingAnswer  , RemoteOffer(g, polite)  ↦ Idle              , SendAnswer(g)     -- glare, yield
//!   AwaitingAnswer  , RemoteOffer(_, impolite)↦ AwaitingAnswer    , Ignore           -- glare, hold
//!   AwaitingAnswer(g), RemoteAnswer(g)        ↦ Idle              , AcceptAnswer
//!   *               , RemoteAnswer(_)         ↦ *                 , Ignore           -- stale
//! ```

/// Per-peer renegotiation signaling state. A connection is either quiescent ([`Idle`]) or has one
/// locally-initiated offer outstanding ([`AwaitingAnswer`]); no other local offer may start until
/// that resolves, so at most one offer is ever in flight from this side.
///
/// [`Idle`]: Negotiation::Idle
/// [`AwaitingAnswer`]: Negotiation::AwaitingAnswer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    /// No local offer outstanding.
    Idle,
    /// A local offer of this `generation` was sent; its answer is awaited. A `RemoteAnswer` is
    /// applied only if it carries this same generation.
    AwaitingAnswer {
        /// Generation id of the outstanding local offer.
        generation: u64,
    },
}

/// An input to [`Negotiator::step`]. The `polite` flag on [`RemoteOffer`](NegotiationEvent::RemoteOffer)
/// is decided by the shell from the two peers' dids (see [`Negotiator::polite`]); it resolves glare
/// deterministically — exactly one side yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationEvent {
    /// The application attached a track and wants to renegotiate.
    LocalRenegotiate,
    /// A renegotiation offer arrived from the peer (`RenegotiateSend`).
    RemoteOffer {
        /// The offer's generation id (echoed back in the answer).
        generation: u64,
        /// Whether *this* node is the polite peer, i.e. the one that yields under glare.
        polite: bool,
    },
    /// A renegotiation answer arrived from the peer (`RenegotiateReport`).
    RemoteAnswer {
        /// The generation id of the offer this answer responds to.
        generation: u64,
    },
}

/// The action the shell must carry out after a [`step`](Negotiator::step). Everything that touches
/// WebRTC or the network lives here, never in the state machine itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiationEffect {
    /// Create a local offer tagged with `generation` and send it as `RenegotiateSend`.
    SendOffer {
        /// Generation id to stamp on the offer.
        generation: u64,
    },
    /// Answer the remote offer of `generation` and send it back as `RenegotiateReport`.
    SendAnswer {
        /// Generation id to echo on the answer (the offer's generation).
        generation: u64,
    },
    /// Apply the remote answer to the outstanding offer.
    AcceptAnswer,
    /// Do nothing — a stale answer, or a remote offer this (impolite) side holds through glare.
    Ignore,
    /// A local renegotiation is already outstanding; the caller must not start another.
    Busy,
}

/// One peer's [`Negotiation`] state plus the monotonic counter that stamps each local offer. Held
/// behind a per-peer lock by the shell so that [`step`](Self::step) calls are serialized.
#[derive(Debug)]
pub struct Negotiator {
    state: Negotiation,
    next_generation: u64,
}

impl Default for Negotiator {
    fn default() -> Self {
        Self {
            state: Negotiation::Idle,
            next_generation: 0,
        }
    }
}

impl Negotiator {
    /// A fresh negotiator in [`Negotiation::Idle`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The current signaling state (for tests / diagnostics).
    pub fn state(&self) -> Negotiation {
        self.state
    }

    /// Undo a committed transition when the shell rejected the operation *before any WebRTC signaling
    /// effect ran* — e.g. a media-track attach failed (data-only connection / kind mismatch) after
    /// `LocalRenegotiate` was admitted but before any offer was created. Restoring the prior state
    /// frees the slot so the next renegotiation is not stuck `Busy`.
    ///
    /// This is **not** used to paper over a *failed WebRTC effect*: once `create_offer` /
    /// `answer_offer` has run, `setLocalDescription` / `setRemoteDescription` has already mutated the
    /// `PeerConnection`, and rolling back only this pure state would make it lie about the real
    /// signaling state. The shell resets the connection in that case instead (see
    /// `SwarmTransport::reset_failed_renegotiation`).
    ///
    /// The monotonic generation counter is intentionally *not* rewound: a spent generation is simply
    /// skipped, which keeps ids unique (gaps are harmless).
    pub fn rollback(&mut self, to: Negotiation) {
        self.state = to;
    }

    /// Which of two peers is the *polite* one — the side that yields its own offer to accept the
    /// other's under glare. We pick the numerically larger did, matching the initial-connection
    /// glare rule in [`crate::swarm::transport::SwarmTransport::answer_remote_connection`] (the
    /// larger did abandons its own offer), so the two negotiation paths break ties the same way.
    pub fn polite(local: crate::dht::Did, remote: crate::dht::Did) -> bool {
        local > remote
    }

    /// The pure transition. Returns the effect the shell must perform; `self` is advanced to the
    /// next state. See the module docs for the full table.
    pub fn step(&mut self, event: NegotiationEvent) -> NegotiationEffect {
        match (self.state, event) {
            // Start a local renegotiation: allocate a fresh generation and await its answer.
            (Negotiation::Idle, NegotiationEvent::LocalRenegotiate) => {
                let generation = self.next_generation;
                self.next_generation += 1;
                self.state = Negotiation::AwaitingAnswer { generation };
                NegotiationEffect::SendOffer { generation }
            }
            // One local offer is already outstanding; refuse to start a second.
            (Negotiation::AwaitingAnswer { .. }, NegotiationEvent::LocalRenegotiate) => {
                NegotiationEffect::Busy
            }
            // No local offer in flight: just answer the remote offer.
            (Negotiation::Idle, NegotiationEvent::RemoteOffer { generation, .. }) => {
                NegotiationEffect::SendAnswer { generation }
            }
            // Glare: both sides offered. The polite side drops its own pending offer and answers the
            // remote one; its own (now abandoned) answer will arrive later and be ignored as stale.
            (
                Negotiation::AwaitingAnswer { .. },
                NegotiationEvent::RemoteOffer {
                    generation,
                    polite: true,
                },
            ) => {
                self.state = Negotiation::Idle;
                NegotiationEffect::SendAnswer { generation }
            }
            // Glare: the impolite side holds its offer and ignores the remote one; the polite peer
            // will answer ours.
            (
                Negotiation::AwaitingAnswer { .. },
                NegotiationEvent::RemoteOffer { polite: false, .. },
            ) => NegotiationEffect::Ignore,
            // The awaited answer for the current generation: apply it and go quiescent.
            (
                Negotiation::AwaitingAnswer { generation },
                NegotiationEvent::RemoteAnswer {
                    generation: answer_generation,
                },
            ) if answer_generation == generation => {
                self.state = Negotiation::Idle;
                NegotiationEffect::AcceptAnswer
            }
            // Any other answer (wrong generation, or none outstanding) is stale.
            (_, NegotiationEvent::RemoteAnswer { .. }) => NegotiationEffect::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Negotiation;
    use super::NegotiationEffect;
    use super::NegotiationEvent;
    use super::Negotiator;

    #[test]
    fn local_renegotiate_from_idle_sends_offer_and_awaits() {
        let mut n = Negotiator::new();
        assert_eq!(
            n.step(NegotiationEvent::LocalRenegotiate),
            NegotiationEffect::SendOffer { generation: 0 }
        );
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn second_local_renegotiate_is_busy() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate);
        assert_eq!(
            n.step(NegotiationEvent::LocalRenegotiate),
            NegotiationEffect::Busy
        );
        // state is unchanged: still awaiting the first offer's answer
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn matching_answer_is_accepted_and_returns_to_idle() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate);
        assert_eq!(
            n.step(NegotiationEvent::RemoteAnswer { generation: 0 }),
            NegotiationEffect::AcceptAnswer
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn stale_answer_wrong_generation_is_ignored() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate); // generation 0 outstanding
        assert_eq!(
            n.step(NegotiationEvent::RemoteAnswer { generation: 7 }),
            NegotiationEffect::Ignore
        );
        // still awaiting the real answer
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn answer_with_no_outstanding_offer_is_ignored() {
        let mut n = Negotiator::new();
        assert_eq!(
            n.step(NegotiationEvent::RemoteAnswer { generation: 0 }),
            NegotiationEffect::Ignore
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn remote_offer_while_idle_is_answered() {
        let mut n = Negotiator::new();
        assert_eq!(
            n.step(NegotiationEvent::RemoteOffer {
                generation: 3,
                polite: false
            }),
            NegotiationEffect::SendAnswer { generation: 3 }
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn glare_polite_side_yields_and_answers() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate); // we offered, generation 0
        assert_eq!(
            n.step(NegotiationEvent::RemoteOffer {
                generation: 5,
                polite: true
            }),
            NegotiationEffect::SendAnswer { generation: 5 }
        );
        // we abandoned our own offer
        assert_eq!(n.state(), Negotiation::Idle);
        // the late answer to our abandoned offer is now stale
        assert_eq!(
            n.step(NegotiationEvent::RemoteAnswer { generation: 0 }),
            NegotiationEffect::Ignore
        );
    }

    #[test]
    fn glare_impolite_side_holds_its_offer() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate); // generation 0
        assert_eq!(
            n.step(NegotiationEvent::RemoteOffer {
                generation: 5,
                polite: false
            }),
            NegotiationEffect::Ignore
        );
        // we keep waiting for the peer to answer our offer
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
        assert_eq!(
            n.step(NegotiationEvent::RemoteAnswer { generation: 0 }),
            NegotiationEffect::AcceptAnswer
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn generations_are_monotonic_across_renegotiations() {
        let mut n = Negotiator::new();
        n.step(NegotiationEvent::LocalRenegotiate);
        n.step(NegotiationEvent::RemoteAnswer { generation: 0 });
        assert_eq!(
            n.step(NegotiationEvent::LocalRenegotiate),
            NegotiationEffect::SendOffer { generation: 1 }
        );
    }

    #[test]
    fn polite_is_the_larger_did() {
        let small = crate::dht::Did::from(crate::ecc::SecretKey::random().address());
        let large = crate::dht::Did::from(crate::ecc::SecretKey::random().address());
        let (small, large) = if small < large {
            (small, large)
        } else {
            (large, small)
        };
        assert!(Negotiator::polite(large, small));
        assert!(!Negotiator::polite(small, large));
    }
}
