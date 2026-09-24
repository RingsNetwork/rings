//! Rerouting: one user-facing DHT send, retried from fresh topology after a pre-acceptance
//! refusal, woken by an event and bounded by a budget.
//!
//! A user operation (`storage_fetch`, `operate_entry`) sends one message per remote placement.
//! A send can race a connection-generation replacement (glare, rejoin, stabilization
//! auto-connect) and be refused before the backend accepted it. This module is the pure
//! automaton that decides what happens next, and the transport shell that performs its two
//! effects: one send, and one wait.
//!
//! # State model
//!
//! ```text
//! Compute(topology) ──route──▶ Local ─────────────────────────────▶ Done(local result)
//!        │
//!        └─▶ Remote(next) ─▶ Send(hop, g) ─▶ Accepted ──────────▶ Done(Ok)
//!                                  │        ├─▶ Failed(e) ─────────▶ Done(Err e)
//!                                  │        └─▶ Deferred(d)
//!                                  │              ├─ deferrals ≤ B ─▶ Waiting(hop, g, demand, d)
//!                                  │              │                     └─Triggered─▶ Compute
//!                                  │              └─ deferrals > B ─▶ Exhausted(last = d)
//! Env      ≜ Replace(g) ∨ Glare(g) ∨ Retire(g) ∨ TopologyChange ∨ Congest ∨ Jam
//! Protocol ≜ Ready(g) ∨ Close(g) ∨ Release ∨ Drain            \* weakly fair (L1)
//! ```
//!
//! `Compute` and `Send` are the effects of the caller (`storage::rerouted`); the verdict of a
//! send is classified by [`Verdict::remote`] through the total `Error::send_class`, whose module
//! proves every `Deferrable` class pre-acceptance. [`Rerouting::after`] is `δ` on verdicts and
//! [`Awaiting::is_triggered`] the guard of `Waiting → Compute`; both are pure. A capacity
//! refusal waits for `Room(hop, demand)`: the admission rule itself, evaluated on the peer and
//! global states without admitting, so it holds exactly when the refused request would now be
//! admitted, whichever scope refused it. A channel refusal waits for its peer's progress,
//! counted against the peer release epoch read *after* the refusal was published: whatever
//! the refused send held (a transfer permit, or the peer half of a reservation) is released
//! before its refusal is published, so every release counted is another transfer's.
//!
//! # Laws
//!
//! ```text
//! (S1) No duplicate effect.
//!      Pre : every Deferred(d) carries a SendDeferral, built only from a detached Cancelled
//!            that passed the cancellation gate of lemma (P), or from an error with
//!            send_class = Deferrable (type invariant of SendDeferral).
//!      Law : Effects(op, placement) ≤ 1.
//!      Proof: an attempt is retried only from Deferred(d) (δ has no other edge to Waiting);
//!            PreAcceptance(d) (send_class lemmas) ⇒ the attempt had no remote effect; a
//!            Local attempt and an Accepted or Failed verdict end the automaton. So at most
//!            one attempt per placement can have an effect, append, tombstone and compact
//!            included.
//! (S2) Fresh hop.
//!      Law : the hop of every send after Waiting is computed from topology read after the
//!            event that satisfied is_triggered.
//!      Proof: the shell leaves Waiting only when is_triggered holds for a route computed
//!            after its listeners were registered; the next send takes that route, and
//!            attempt_remote binds its link hop from the connection table read at the send.
//!            No hop survives a deferral: Awaiting keeps the old hop only to compare against.
//! (S3) Bounded.
//!      Inv : deferrals ≤ REROUTING_BUDGET in every Rerouting and Awaiting.
//!      Law : sends(op, placement) ≤ REROUTING_BUDGET + 1, and exhaustion returns
//!            Error::ReroutingExhausted { last } with the last deferral's cause.
//! (L1) Liveness.
//!      Premise: TopologyReferencesOnlyAdmitted (#772): topology routes only to peers with an
//!            admitted generation, so a routed hop is usable, recovering, or retired and
//!            removed from the topology (a route change).
//!      Fairness: WF(Ready(g)) for every registered generation g of a routed hop; WF(Close)
//!            of a dead generation; WF(Release) and WF(Drain) while a transfer holds capacity
//!            or a channel; every such step advances an epoch the shell listens to. A channel
//!            that holds none of this node's frames and still refuses them is broken; the
//!            liveness layer retires it (a link change), outside this law's environment.
//!      Law : if Env stops while deferrals ≤ REROUTING_BUDGET − QUIESCENT_DEFERRALS, then
//!            ◇ Done(Ok): a ready responsible hop is reached within the budget.
//!      Proof: after Env stops, at most QUIESCENT_DEFERRALS refusals remain (its derivation);
//!            each is followed by an event its trigger names, which satisfies is_triggered,
//!            and a send to a usable hop with capacity and a drained channel is accepted. The
//!            model check decides this on every fair trace and finds a counterexample with one
//!            deferral less of budget.
//! ```
//!
//! The model check `test_model` enumerates every bounded trace of this automaton composed
//! with the production lifecycle registry and asserts S1–S3 on all of them and L1 on the fair
//! ones.

use event_listener::EventListener;
use futures::future::select_all;
use serde::Serialize;

use super::delivery::SendCompletionOutcome;
use super::outbound::PeerProgress;
use super::outbound::PeerStamp;
use super::outbound::TransferDemand;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::DeferralTrigger;
use crate::error::Error;
use crate::error::Result;
use crate::error::SendDeferral;
use crate::message::PayloadSender;

/// Deferrals a placement can still meet after the environment stops (tight: the model check
/// finds a fair exhausting trace with one deferral less of budget).
///
/// ```text
/// 1. the in-flight send's generation was superseded or died       (Superseded, Missing, …)
/// 2. outbound capacity of the routed hop is still exhausted          (CapacityExceeded, …)
/// 3. the channel of the hop the route settles on is still busy     (QueueTimeout)
/// 4. the replacement generation is admitted (WF readiness), the
///    route moves to it, and its channel is busy as well            (QueueTimeout)
/// ```
///
/// A capacity refusal wakes only once its request has `Room`, and a channel refusal on its
/// channel's progress; once the environment stops, neither condition is re-established, so
/// each costs at most one deferral per route. While the environment keeps filling a channel,
/// a channel refusal may cost one deferral per partial drain: the budget bounds that case,
/// and the caller may drop the operation at any wait.
pub(crate) const QUIESCENT_DEFERRALS: u8 = 4;

/// Deferrals one generation replacement racing the operation causes: the death of the bound
/// generation refuses the send once, after which the route moves to a usable hop (the
/// replacement once admitted, or another). Tight: the model check's `one replacement`
/// configuration reaches exactly this many and no more.
pub(crate) const REPLACEMENT_DEFERRALS: u8 = 1;

/// Deferred sends one placement tolerates before `Exhausted`: what one generation replacement
/// racing the operation spends, plus what L1 needs once the environment stops.
pub(crate) const REROUTING_BUDGET: u8 = REPLACEMENT_DEFERRALS + QUIESCENT_DEFERRALS;

/// The first link hop toward a destination, as the connection table shows it now.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct LinkHop {
    /// Link peer a send would be bound to.
    pub(crate) hop: Did,
    /// The hop's sendable generation, if any.
    pub(crate) generation: Option<u64>,
    /// Whether that generation can make progress now.
    pub(crate) usable: bool,
}

/// Where a placement's route leads now, as the wake guard reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum LinkRoute {
    /// This node holds the placement.
    Local,
    /// Through the link hop.
    Remote(LinkHop),
}

/// What the wake guard reads of local resources now.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct Observation {
    /// `Room(hop, demand)`: the refused request would now be admitted.
    pub(crate) room: bool,
    /// The refused hop's peer capacity against the stamp taken after the refusal.
    pub(crate) peer: PeerProgress,
}

/// The classified outcome of one attempt.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// Executed locally, or accepted by the backend: the attempt's effect happened.
    Accepted,
    /// Refused before backend acceptance by the send bound to `hop`.
    Deferred {
        /// Link peer the refused send was bound to.
        hop: Did,
        /// The hop's sendable generation when the send was prepared.
        generation: Option<u64>,
        /// What the refused transfer asked of outbound capacity.
        demand: TransferDemand,
        /// The refusal, a witness of `PreAcceptance`.
        cause: SendDeferral,
    },
    /// Ambiguous or fatal: never retried.
    Failed(Error),
}

impl Verdict {
    /// Classify a local attempt: whatever happened locally is final.
    pub(crate) fn local(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self::Accepted,
            Err(error) => Self::Failed(error),
        }
    }

    /// Classify a detached send of `demand` bound to `hop` under `generation`.
    ///
    /// `Ok(Cancelled)` is pre-acceptance by the cancellation gate of lemma (P); an error defers
    /// iff its `send_class` is `Deferrable`.
    fn remote(
        hop: Did,
        generation: Option<u64>,
        demand: TransferDemand,
        result: Result<SendCompletionOutcome>,
    ) -> Self {
        let cause = match result {
            Ok(SendCompletionOutcome::Succeeded) => return Self::Accepted,
            Ok(SendCompletionOutcome::Cancelled) => SendDeferral::cancelled(hop),
            Err(error) => match SendDeferral::refused(error) {
                Ok(cause) => cause,
                Err(error) => return Self::Failed(error),
            },
        };
        Self::Deferred {
            hop,
            generation,
            demand,
            cause,
        }
    }
}

/// The automaton between attempts: how many deferrals this placement has spent.
///
/// Inv: `deferrals ≤ REROUTING_BUDGET`.
#[derive(Debug)]
pub(crate) struct Rerouting {
    /// Deferred sends so far.
    deferrals: u8,
}

/// The result of one transition.
#[derive(Debug)]
pub(crate) enum Step {
    /// The attempt's effect happened; the placement is done.
    Complete,
    /// The placement failed: a fatal or ambiguous verdict, or `ReroutingExhausted`.
    Fail(Error),
    /// Wait for the trigger, then compute again.
    Await(Awaiting),
}

/// `Waiting(hop, generation, demand, cause)`: a deferral within budget, awaiting its trigger.
///
/// Inv: `deferrals ≤ REROUTING_BUDGET`.
#[derive(Debug)]
pub(crate) struct Awaiting {
    /// Deferred sends so far, this one included.
    deferrals: u8,
    /// Link peer of the refused send.
    hop: Did,
    /// The hop's sendable generation when the refused send was prepared.
    generation: Option<u64>,
    /// What the refused transfer asked of outbound capacity.
    demand: TransferDemand,
    /// The refusal.
    cause: SendDeferral,
}

impl Rerouting {
    /// The initial state: no deferral spent.
    pub(crate) const fn start() -> Self {
        Self { deferrals: 0 }
    }

    /// `δ(R, verdict)`.
    ///
    /// Post: `Complete` iff `Accepted`; `Fail(e)` for `Failed(e)`; a deferral within budget
    /// awaits, and the deferral past it fails with `ReroutingExhausted` carrying its cause.
    pub(crate) fn after(self, verdict: Verdict) -> Step {
        match verdict {
            Verdict::Accepted => Step::Complete,
            Verdict::Failed(error) => Step::Fail(error),
            Verdict::Deferred {
                hop,
                generation,
                demand,
                cause,
            } => {
                let deferrals = self.deferrals.saturating_add(1);
                if deferrals > REROUTING_BUDGET {
                    return Step::Fail(Error::ReroutingExhausted { last: cause });
                }
                Step::Await(Awaiting {
                    deferrals,
                    hop,
                    generation,
                    demand,
                    cause,
                })
            }
        }
    }
}

impl Awaiting {
    /// The guard of `Waiting → Compute` over a route recomputed now and an observation now.
    ///
    /// ```text
    /// Moved     ≜ route ≠ Remote(hop, _, _)
    /// Replaced  ≜ route = Remote(hop, g', _) ∧ g' ≠ generation
    /// Progress  ≜ another transfer of hop's peer released since the refusal
    /// Triggered ≜ Moved ∨ (trigger = LinkChange      ∧ route = Remote(hop, _, usable))
    ///                   ∨ (trigger = CapacityRelease ∧ Room(hop, demand))
    ///                   ∨ (trigger = ChannelDrain    ∧ (Replaced ∨ Progress ∨ Idle(hop)))
    /// ```
    ///
    /// `Room` is exact for the scope that refused (the peer's slots or bytes, or the global
    /// bytes): a release in another scope, or one too small, does not satisfy it. A channel
    /// refusal resolves as the channel makes progress, not only when it empties, so a busy but
    /// flowing channel wakes the placement. Every disjunct is a predicate over readings taken
    /// after the listeners were registered, so no wake-up is lost, and none is satisfied by the
    /// refused send's own release (module documentation).
    pub(crate) fn is_triggered(&self, route: LinkRoute, observation: Observation) -> bool {
        let LinkRoute::Remote(LinkHop {
            hop,
            generation,
            usable,
        }) = route
        else {
            return true;
        };
        hop != self.hop
            || match self.cause.trigger() {
                DeferralTrigger::LinkChange => usable,
                DeferralTrigger::CapacityRelease => observation.room,
                DeferralTrigger::ChannelDrain => {
                    generation != self.generation
                        || observation.peer.released
                        || observation.peer.idle
                }
            }
    }

    /// Leave `Waiting` for `Compute`, keeping the deferrals spent.
    pub(crate) fn resume(self) -> Rerouting {
        Rerouting {
            deferrals: self.deferrals,
        }
    }
}

impl SwarmTransport {
    /// Record that a transport callback observed a connection or data-channel state change.
    ///
    /// An active generation recovering from `Disconnected` has no lifecycle transition, so its
    /// callback is the only event that can make a waiting hop usable again.
    pub(crate) fn signal_link_transition(&self) {
        self.connection_lifecycle.link_transitions().advance();
    }

    /// The link hop toward `next` now: the peer `infer_next_hop` binds, its sendable
    /// generation, and whether that generation can make progress.
    pub(crate) fn link_hop(&self, next: Did) -> Result<LinkHop> {
        let hop = self.infer_next_hop(next, None)?;
        let admitted = self.admitted_send_connection(hop)?;
        Ok(LinkHop {
            hop,
            generation: admitted
                .as_ref()
                .map(|admitted| admitted.attempt().generation()),
            usable: admitted
                .is_some_and(|admitted| admitted.connection().readiness().can_make_progress()),
        })
    }

    /// Send `message` to `destination` through the hop `infer_next_hop` binds, detached, and
    /// classify the outcome.
    ///
    /// Unlike `PayloadSender::send_message`, a `Cancelled` completion is not reported as a
    /// success: it is the pre-acceptance deferral the cancellation gate proves it to be.
    pub(crate) async fn attempt_remote<T>(&self, message: T, destination: Did) -> Verdict
    where T: Serialize + Send {
        let LinkHop {
            hop, generation, ..
        } = match self.link_hop(destination) {
            Ok(link) => link,
            Err(error) => return Verdict::Failed(error),
        };
        let payload = match self.signed_payload(message, hop, destination).await {
            Ok(payload) => payload,
            Err(error) => return Verdict::Failed(error),
        };
        let demand = match Self::payload_demand(&payload) {
            Ok(demand) => demand,
            Err(error) => return Verdict::Failed(error),
        };
        let result = self.send_payload_detached_with_outcome(payload).await;
        Verdict::remote(hop, generation, demand, result)
    }

    /// Stamp the release epoch of `awaiting`'s hop, after its refusal was published.
    pub(crate) fn peer_stamp(&self, awaiting: &Awaiting) -> PeerStamp {
        self.outbound_schedulers.peer_stamp(awaiting.hop)
    }

    /// The observation the wake guard of `awaiting` reads now, against the hop's `peer`
    /// stamp.
    pub(crate) fn observation(&self, awaiting: &Awaiting, peer: &PeerStamp) -> Observation {
        Observation {
            room: self
                .outbound_schedulers
                .has_room(awaiting.hop, awaiting.demand),
            peer: peer.progress(),
        }
    }

    /// Test hook: the rerouted placements waiting for a link event (`LinkChange` or
    /// `ChannelDrain`): only a waiting placement listens on the link epoch.
    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn link_waiters_for_test(&self) -> usize {
        self.connection_lifecycle.link_transitions().listeners()
    }

    /// Register for the next event of every class that can trigger `awaiting`.
    ///
    /// ```text
    /// LinkChange      ─▶ topology, link
    /// CapacityRelease ─▶ topology, peer releases, global releases     (Room may follow either)
    /// ChannelDrain    ─▶ topology, link, peer releases
    /// ```
    ///
    /// A peer whose capacity is gone has no listener: its progress already reads idle.
    ///
    /// Pre: the caller evaluates [`Awaiting::is_triggered`] *after* this call and awaits
    /// [`ReroutingListeners::notified`] only when it is false, so no event is lost (`Law (Wake)`
    /// of `Epoch`).
    pub(crate) fn rerouting_listeners(
        &self,
        awaiting: &Awaiting,
        peer: &PeerStamp,
    ) -> ReroutingListeners {
        let topology = self.dht.topology_epoch().listen();
        let link = || self.connection_lifecycle.link_transitions().listen();
        let capacity = || self.outbound_schedulers.capacity_releases().listen();
        let mut listeners = match awaiting.cause.trigger() {
            DeferralTrigger::LinkChange => vec![topology, link()],
            DeferralTrigger::CapacityRelease => vec![topology, capacity()],
            DeferralTrigger::ChannelDrain => vec![topology, link()],
        };
        if awaiting.cause.trigger() != DeferralTrigger::LinkChange {
            listeners.extend(peer.listen());
        }
        ReroutingListeners(listeners)
    }
}

/// One registration on the epochs a waiting placement's trigger reads.
pub(crate) struct ReroutingListeners(Vec<EventListener>);

impl ReroutingListeners {
    /// Resolve at the first event of any listened class. No duration bounds this wait: it ends
    /// by an event (L1's fairness), or when the caller drops the operation.
    pub(crate) async fn notified(self) {
        select_all(self.0).await;
    }
}

#[cfg(test)]
mod test_model;

#[cfg(test)]
mod tests {
    use super::Awaiting;
    use super::LinkHop;
    use super::LinkRoute;
    use super::Observation;
    use super::PeerProgress;
    use super::Rerouting;
    use super::Step;
    use super::TransferDemand;
    use super::Verdict;
    use super::REROUTING_BUDGET;
    use crate::dht::Did;
    use crate::error::Error;
    use crate::error::SendDeferral;

    /// The deferral of a send to `hop` refused because its generation was superseded.
    fn superseded(hop: Did, generation: u64) -> Verdict {
        Verdict::remote(
            hop,
            Some(generation),
            TransferDemand::for_test(),
            Err(Error::ConnectionAttemptSuperseded {
                peer: hop,
                generation,
            }),
        )
    }

    /// An awaiting state for `hop` under generation `1`, deferred by `cause`.
    fn awaiting(hop: Did, cause: SendDeferral) -> Awaiting {
        Awaiting {
            deferrals: 1,
            hop,
            generation: Some(1),
            demand: TransferDemand::for_test(),
            cause,
        }
    }

    /// The deferral `error` classifies into.
    fn deferral(error: Error) -> SendDeferral {
        match SendDeferral::refused(error) {
            Ok(cause) => cause,
            Err(error) => panic!("{error} must defer"),
        }
    }

    /// A route through `hop` under `generation`.
    const fn through(hop: Did, generation: u64, usable: bool) -> LinkRoute {
        LinkRoute::Remote(LinkHop {
            hop,
            generation: Some(generation),
            usable,
        })
    }

    /// An observation of `room` and the hop's peer progress.
    const fn observed(room: bool, released: bool, idle: bool) -> Observation {
        Observation {
            room,
            peer: PeerProgress { released, idle },
        }
    }

    /// Law (S3): `REROUTING_BUDGET` deferrals await; the next fails with `ReroutingExhausted`
    /// carrying the last cause.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_budget_bounds_deferrals_and_exhaustion_carries_the_last_cause() {
        let hop = Did::from(7_u32);
        let mut rerouting = Rerouting::start();
        for generation in 1..=u64::from(REROUTING_BUDGET) {
            let Step::Await(awaiting) = rerouting.after(superseded(hop, generation)) else {
                panic!("deferral {generation} is within budget");
            };
            rerouting = awaiting.resume();
        }
        let last = u64::from(REROUTING_BUDGET) + 1;
        let Step::Fail(Error::ReroutingExhausted { last: cause }) =
            rerouting.after(superseded(hop, last))
        else {
            panic!("the deferral past the budget exhausts");
        };
        let expected = Error::ConnectionAttemptSuperseded {
            peer: hop,
            generation: last,
        };
        assert_eq!(cause.to_string(), expected.to_string());
    }

    /// Law (S1): only a deferral awaits; acceptance completes and an ambiguous failure is
    /// returned unretried.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_only_a_pre_acceptance_refusal_awaits() {
        let hop = Did::from(7_u32);
        assert!(matches!(
            Rerouting::start().after(Verdict::Accepted),
            Step::Complete
        ));
        let ambiguous = Error::DetachedSendAbandonedAfterClaim { peer: hop };
        assert!(matches!(
            Rerouting::start().after(Verdict::remote(
                hop,
                Some(1),
                TransferDemand::for_test(),
                Err(ambiguous)
            )),
            Step::Fail(Error::DetachedSendAbandonedAfterClaim { .. })
        ));
    }

    /// Law (S2): each trigger wakes on its own event and on a route move, and on nothing else.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_triggers_wake_on_their_events_and_route_moves_only() {
        let hop = Did::from(7_u32);
        let moved = through(Did::from(8_u32), 1, false);

        let link = awaiting(hop, SendDeferral::cancelled(hop));
        assert!(!link.is_triggered(through(hop, 1, false), observed(true, true, true)));
        assert!(link.is_triggered(through(hop, 1, true), observed(false, false, false)));
        assert!(link.is_triggered(moved, observed(false, false, false)));
        assert!(link.is_triggered(LinkRoute::Local, observed(false, false, false)));

        let capacity = awaiting(
            hop,
            deferral(Error::OutboundTransferCapacityExceeded {
                peer: hop,
                capacity: 1,
            }),
        );
        // Releases that leave no room (another scope, or too little) do not wake it.
        assert!(!capacity.is_triggered(through(hop, 1, true), observed(false, true, true)));
        assert!(capacity.is_triggered(through(hop, 1, true), observed(true, false, false)));
        assert!(capacity.is_triggered(moved, observed(false, false, false)));

        let drain = awaiting(
            hop,
            deferral(Error::DataChannelSendQueueTimeout {
                peer: hop,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            }),
        );
        assert!(!drain.is_triggered(through(hop, 1, true), observed(true, false, false)));
        assert!(drain.is_triggered(through(hop, 1, true), observed(false, true, false)));
        assert!(drain.is_triggered(through(hop, 1, true), observed(false, false, true)));
        assert!(drain.is_triggered(through(hop, 2, true), observed(false, false, false)));
        assert!(drain.is_triggered(moved, observed(false, false, false)));
    }
}
