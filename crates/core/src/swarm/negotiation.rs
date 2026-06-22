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
//! This module is the **functional core** that prevents that, as a genuine pure transition with an
//! explicit commit point — not a mutate-then-undo:
//!
//! - [`Negotiator::decide`] is `(state, event) ↦ Decision`. It borrows `&self`, mutates nothing, and
//!   does no I/O; it answers *what should happen* using only the event (an offer/answer generation,
//!   a politeness bit) and the current state.
//! - [`Negotiator::commit`] is the **only** mutation. The shell calls it *after* the effect a
//!   [`Decision`] authorized has actually succeeded. If the effect fails, `commit` is never called,
//!   so the state never has to be rolled back — a failed `create_offer`/`setRemoteDescription`/send
//!   simply leaves the negotiator where it was.
//!
//! The shell (`crate::swarm::transport`) holds one [`Negotiator`] per peer behind a lock, so the
//! `decide → run effect → commit` sequence is serialized: only one local renegotiation is ever
//! outstanding, every offer carries a monotonic generation, and an answer whose generation does not
//! match the outstanding offer is dropped.
//!
//! ```text
//!   decide : Negotiation × Event ↦ Decision         -- pure, no mutation
//!   commit : Negotiation × Decision ↦ Negotiation   -- applied only after the effect succeeds
//!
//!   Idle             , LocalRenegotiate         ↦ SendOffer(g)     then commit AwaitingAnswer(g)
//!   AwaitingAnswer   , LocalRenegotiate         ↦ Busy             -- serialize; nothing to commit
//!   Idle             , RemoteOffer(g, _)        ↦ SendAnswer(g)    then commit Idle
//!   AwaitingAnswer   , RemoteOffer(g, polite)   ↦ SendAnswer(g)    then commit Idle  -- glare, yield
//!   AwaitingAnswer   , RemoteOffer(_, impolite) ↦ Ignore           -- glare, hold; nothing to commit
//!   AwaitingAnswer(g), RemoteAnswer(g)          ↦ AcceptAnswer     then commit Idle
//!   *                , RemoteAnswer(_)          ↦ Ignore           -- stale; nothing to commit
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

/// An input to [`Negotiator::decide`]. The `polite` flag on [`RemoteOffer`](NegotiationEvent::RemoteOffer)
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

/// What [`Negotiator::decide`] resolved an event to. The acting variants name the effect the shell
/// must run; [`Negotiator::commit`] interprets the same value to advance the state once that effect
/// succeeds. The non-acting variants ([`Ignore`](Decision::Ignore), [`Busy`](Decision::Busy)) carry
/// no effect and are never committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Do nothing — a stale/duplicate answer, or a remote offer this (impolite) side holds through
    /// glare. The connection is not touched.
    Ignore,
    /// A local renegotiation is already outstanding; the caller must not start another.
    Busy,
    /// Create a local offer tagged with `generation` and send it; on success commit
    /// [`AwaitingAnswer`](Negotiation::AwaitingAnswer).
    SendOffer {
        /// Generation id to stamp on the offer.
        generation: u64,
    },
    /// Answer the remote offer of `generation` and send it back; on success commit
    /// [`Idle`](Negotiation::Idle).
    SendAnswer {
        /// Generation id to echo on the answer (the offer's generation).
        generation: u64,
    },
    /// Apply the remote answer to the outstanding offer; on success commit [`Idle`](Negotiation::Idle).
    AcceptAnswer,
}

/// One peer's [`Negotiation`] state plus the monotonic counter that stamps each local offer. Held
/// behind a per-peer lock by the shell so that the `decide → effect → commit` sequence is serialized.
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

    /// Which of two peers is the *polite* one — the side that yields its own offer to accept the
    /// other's under glare. We pick the numerically larger did, matching the initial-connection
    /// glare rule in `SwarmTransport::answer_remote_connection` (the
    /// larger did abandons its own offer), so the two negotiation paths break ties the same way.
    pub fn polite(local: crate::dht::Did, remote: crate::dht::Did) -> bool {
        local > remote
    }

    /// The pure decision: resolve `event` against the current state to a [`Decision`], **without**
    /// mutating anything. See the module docs for the full table.
    pub fn decide(&self, event: NegotiationEvent) -> Decision {
        match (self.state, event) {
            // Start a local renegotiation (only one may be outstanding at a time).
            (Negotiation::Idle, NegotiationEvent::LocalRenegotiate) => Decision::SendOffer {
                generation: self.next_generation,
            },
            (Negotiation::AwaitingAnswer { .. }, NegotiationEvent::LocalRenegotiate) => {
                Decision::Busy
            }
            // No local offer in flight: just answer the remote offer.
            (Negotiation::Idle, NegotiationEvent::RemoteOffer { generation, .. }) => {
                Decision::SendAnswer { generation }
            }
            // Glare: both offered. The polite side yields its own offer and answers the remote one;
            // the impolite side holds its offer and ignores the remote one.
            (
                Negotiation::AwaitingAnswer { .. },
                NegotiationEvent::RemoteOffer {
                    generation,
                    polite: true,
                },
            ) => Decision::SendAnswer { generation },
            (
                Negotiation::AwaitingAnswer { .. },
                NegotiationEvent::RemoteOffer { polite: false, .. },
            ) => Decision::Ignore,
            // The awaited answer for the current generation.
            (
                Negotiation::AwaitingAnswer { generation },
                NegotiationEvent::RemoteAnswer {
                    generation: answer_generation,
                },
            ) if answer_generation == generation => Decision::AcceptAnswer,
            // Any other answer (wrong generation, or none outstanding) is stale.
            (_, NegotiationEvent::RemoteAnswer { .. }) => Decision::Ignore,
        }
    }

    /// Apply a [`Decision`] whose effect has **succeeded**, advancing the state. The shell calls this
    /// only on the success path, so there is no rollback: a failed effect simply never commits.
    ///
    /// [`Ignore`](Decision::Ignore) / [`Busy`](Decision::Busy) carry no effect and must not be
    /// committed; committing one is a caller bug and is a no-op here.
    pub fn commit(&mut self, decision: Decision) {
        match decision {
            Decision::SendOffer { generation } => {
                self.state = Negotiation::AwaitingAnswer { generation };
                // Spend the generation so the next offer gets a fresh, larger id.
                self.next_generation = generation + 1;
            }
            // Answering a remote offer (incl. the glare yield) and accepting an answer both leave the
            // connection with no local offer outstanding.
            Decision::SendAnswer { .. } | Decision::AcceptAnswer => {
                self.state = Negotiation::Idle;
            }
            Decision::Ignore | Decision::Busy => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Decision;
    use super::Negotiation;
    use super::NegotiationEvent;
    use super::Negotiator;

    /// Decide, assert the decision, then commit it — the shell's success-path sequence.
    fn decide_commit(n: &mut Negotiator, event: NegotiationEvent) -> Decision {
        let decision = n.decide(event);
        n.commit(decision);
        decision
    }

    #[test]
    fn decide_does_not_mutate() {
        let n = Negotiator::new();
        let _ = n.decide(NegotiationEvent::LocalRenegotiate);
        // No commit, so the state is unchanged — `decide` is pure.
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn local_renegotiate_from_idle_sends_offer_then_commits_awaiting() {
        let mut n = Negotiator::new();
        assert_eq!(
            decide_commit(&mut n, NegotiationEvent::LocalRenegotiate),
            Decision::SendOffer { generation: 0 }
        );
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn second_local_renegotiate_is_busy() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate);
        assert_eq!(n.decide(NegotiationEvent::LocalRenegotiate), Decision::Busy);
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn failed_offer_is_never_committed_so_no_rollback_is_needed() {
        let n = Negotiator::new();
        // Decide an offer but do NOT commit (simulating a failed create_offer/send).
        assert_eq!(
            n.decide(NegotiationEvent::LocalRenegotiate),
            Decision::SendOffer { generation: 0 }
        );
        assert_eq!(n.state(), Negotiation::Idle);
        // The generation was not spent: the next attempt reuses it.
        assert_eq!(
            n.decide(NegotiationEvent::LocalRenegotiate),
            Decision::SendOffer { generation: 0 }
        );
    }

    #[test]
    fn matching_answer_is_accepted_and_returns_to_idle() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate);
        assert_eq!(
            decide_commit(&mut n, NegotiationEvent::RemoteAnswer { generation: 0 }),
            Decision::AcceptAnswer
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn stale_answer_wrong_generation_is_ignored() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate); // generation 0 outstanding
        assert_eq!(
            n.decide(NegotiationEvent::RemoteAnswer { generation: 7 }),
            Decision::Ignore
        );
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
    }

    #[test]
    fn answer_with_no_outstanding_offer_is_ignored() {
        let n = Negotiator::new();
        assert_eq!(
            n.decide(NegotiationEvent::RemoteAnswer { generation: 0 }),
            Decision::Ignore
        );
    }

    #[test]
    fn remote_offer_while_idle_is_answered() {
        let mut n = Negotiator::new();
        assert_eq!(
            decide_commit(&mut n, NegotiationEvent::RemoteOffer {
                generation: 3,
                polite: false
            }),
            Decision::SendAnswer { generation: 3 }
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn glare_polite_side_yields_and_answers() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate); // we offered, generation 0
        assert_eq!(
            decide_commit(&mut n, NegotiationEvent::RemoteOffer {
                generation: 5,
                polite: true
            }),
            Decision::SendAnswer { generation: 5 }
        );
        // we abandoned our own offer
        assert_eq!(n.state(), Negotiation::Idle);
        // the late answer to our abandoned offer is now stale
        assert_eq!(
            n.decide(NegotiationEvent::RemoteAnswer { generation: 0 }),
            Decision::Ignore
        );
    }

    #[test]
    fn glare_impolite_side_holds_its_offer() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate); // generation 0
        assert_eq!(
            n.decide(NegotiationEvent::RemoteOffer {
                generation: 5,
                polite: false
            }),
            Decision::Ignore
        );
        assert_eq!(n.state(), Negotiation::AwaitingAnswer { generation: 0 });
        assert_eq!(
            decide_commit(&mut n, NegotiationEvent::RemoteAnswer { generation: 0 }),
            Decision::AcceptAnswer
        );
        assert_eq!(n.state(), Negotiation::Idle);
    }

    #[test]
    fn generations_are_monotonic_across_committed_renegotiations() {
        let mut n = Negotiator::new();
        decide_commit(&mut n, NegotiationEvent::LocalRenegotiate);
        decide_commit(&mut n, NegotiationEvent::RemoteAnswer { generation: 0 });
        assert_eq!(
            n.decide(NegotiationEvent::LocalRenegotiate),
            Decision::SendOffer { generation: 1 }
        );
    }

    #[test]
    fn polite_is_the_larger_did() {
        let a = crate::dht::Did::from(crate::ecc::SecretKey::random().address());
        let b = crate::dht::Did::from(crate::ecc::SecretKey::random().address());
        let (small, large) = if a < b { (a, b) } else { (b, a) };
        assert!(Negotiator::polite(large, small));
        assert!(!Negotiator::polite(small, large));
    }
}
