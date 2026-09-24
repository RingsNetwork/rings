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
//!                                  │              ├─ deferrals ≤ B ─▶ Waiting(hop, g, stamps, d)
//!                                  │              │                     └─Triggered─▶ Compute
//!                                  │              └─ deferrals > B ─▶ Exhausted(last = d)
//! Env ≜ Replace(g) ∨ Glare(g) ∨ Retire(g) ∨ TopologyChange ∨ Ready(g) ∨ Release ∨ Drain
//! ```
//!
//! `Compute` and `Send` are the effects of the caller (`storage::rerouted`); the verdict of a
//! send is classified by [`Verdict::remote`] through the total `Error::send_class`, whose module
//! proves every `Deferrable` class pre-acceptance. [`Rerouting::after`] is `δ` on verdicts and
//! [`Awaiting::is_triggered`] the guard of `Waiting → Compute`; both are pure. No trigger can
//! be satisfied by the refused send itself: a capacity refusal is stamped with the release
//! epoch read *before* the send and held no permit, so every later release is another
//! transfer's; a channel refusal waits for the state `Idle(hop)` (no transfer of the peer in
//! flight), which the refused transfer's own release cannot establish while other transfers
//! hold the channel, and which it establishes only when nothing else does, i.e. when the
//! channel holds none of this node's frames.
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
/// 2. the shared outbound capacity is still exhausted               (AdmissionTimeout)
/// 3. the channel of the hop the route settles on is still busy     (QueueTimeout)
/// 4. the replacement generation is admitted (WF readiness), the
///    route moves to it, and its channel is busy as well            (QueueTimeout)
/// ```
///
/// Each class costs at most one deferral per route: a capacity refusal wakes on a release that
/// clears it, and a channel refusal wakes only once its channel is idle.
pub(crate) const QUIESCENT_DEFERRALS: u8 = 4;

/// Deferrals one generation replacement racing the operation causes: the death of the bound
/// generation, and the admission of its replacement that moves the route mid-send (witnessed
/// by the model check's `one replacement` configuration, which never exhausts).
pub(crate) const REPLACEMENT_DEFERRALS: u8 = 2;

/// Deferred sends one placement tolerates before `Exhausted`: what one generation replacement
/// racing the operation spends, plus what L1 needs once the environment stops.
pub(crate) const REROUTING_BUDGET: u8 = REPLACEMENT_DEFERRALS + QUIESCENT_DEFERRALS;

/// A reading of the capacity-release epoch, taken before a send.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub(crate) struct CapacityStamp(u64);

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
    /// The capacity-release epoch now.
    pub(crate) capacity: CapacityStamp,
    /// `Idle(hop)`: no transfer of the refused hop's peer holds its channel.
    pub(crate) idle: bool,
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

    /// Classify a detached send bound to `hop` under `generation`.
    ///
    /// `Ok(Cancelled)` is pre-acceptance by the cancellation gate of lemma (P); an error defers
    /// iff its `send_class` is `Deferrable`.
    fn remote(hop: Did, generation: Option<u64>, result: Result<SendCompletionOutcome>) -> Self {
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

/// `Waiting(hop, generation, stamp, cause)`: a deferral within budget, awaiting its trigger.
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
    /// Capacity epoch read before the refused send.
    stamp: CapacityStamp,
    /// The refusal.
    cause: SendDeferral,
}

impl Rerouting {
    /// The initial state: no deferral spent.
    pub(crate) const fn start() -> Self {
        Self { deferrals: 0 }
    }

    /// `δ(R, verdict)` for an attempt whose send (if any) was stamped `stamp`.
    ///
    /// Post: `Complete` iff `Accepted`; `Fail(e)` for `Failed(e)`; a deferral within budget
    /// awaits, and the deferral past it fails with `ReroutingExhausted` carrying its cause.
    pub(crate) fn after(self, stamp: CapacityStamp, verdict: Verdict) -> Step {
        match verdict {
            Verdict::Accepted => Step::Complete,
            Verdict::Failed(error) => Step::Fail(error),
            Verdict::Deferred {
                hop,
                generation,
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
                    stamp,
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
    /// Triggered ≜ Moved ∨ (trigger = LinkChange      ∧ route = Remote(hop, _, usable))
    ///                   ∨ (trigger = CapacityRelease ∧ capacity > stamp)
    ///                   ∨ (trigger = ChannelDrain    ∧ (Replaced ∨ Idle(hop)))
    /// ```
    ///
    /// Every disjunct is a state predicate over readings taken now, so a wait that starts after
    /// its event has already happened ends at once. No disjunct holds by the refused send's own
    /// release (module documentation).
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
                DeferralTrigger::CapacityRelease => observation.capacity > self.stamp,
                DeferralTrigger::ChannelDrain => generation != self.generation || observation.idle,
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

    /// The capacity-release epoch now: the stamp of the next send.
    pub(crate) fn capacity_stamp(&self) -> CapacityStamp {
        CapacityStamp(self.outbound_schedulers.capacity_releases().current())
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
        let result = match self.signed_payload(message, hop, destination).await {
            Ok(payload) => self.send_payload_detached_with_outcome(payload).await,
            Err(error) => Err(error),
        };
        Verdict::remote(hop, generation, result)
    }

    /// The observation the wake guard of `awaiting` reads now.
    pub(crate) fn observation(&self, awaiting: &Awaiting) -> Observation {
        Observation {
            capacity: self.capacity_stamp(),
            idle: self.outbound_schedulers.is_idle(awaiting.hop),
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
    /// CapacityRelease ─▶ topology, capacity
    /// ChannelDrain    ─▶ topology, link, capacity   (every peer release precedes a global one)
    /// ```
    ///
    /// Pre: the caller evaluates [`Awaiting::is_triggered`] *after* this call and awaits
    /// [`ReroutingListeners::notified`] only when it is false, so no event is lost (`Law (Wake)`
    /// of `Epoch`).
    pub(crate) fn rerouting_listeners(&self, awaiting: &Awaiting) -> ReroutingListeners {
        let topology = self.dht.topology_epoch().listen();
        let link = || self.connection_lifecycle.link_transitions().listen();
        let capacity = || self.outbound_schedulers.capacity_releases().listen();
        ReroutingListeners(match awaiting.cause.trigger() {
            DeferralTrigger::LinkChange => vec![topology, link()],
            DeferralTrigger::CapacityRelease => vec![topology, capacity()],
            DeferralTrigger::ChannelDrain => vec![topology, link(), capacity()],
        })
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
    use super::CapacityStamp;
    use super::LinkHop;
    use super::LinkRoute;
    use super::Observation;
    use super::Rerouting;
    use super::Step;
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
            Err(Error::ConnectionAttemptSuperseded {
                peer: hop,
                generation,
            }),
        )
    }

    /// An awaiting state for `hop` under generation `1`, stamped `stamp`, deferred by `cause`.
    fn awaiting(hop: Did, stamp: u64, cause: SendDeferral) -> Awaiting {
        Awaiting {
            deferrals: 1,
            hop,
            generation: Some(1),
            stamp: CapacityStamp(stamp),
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

    /// An observation of capacity epoch `capacity` and idleness `idle`.
    const fn observed(capacity: u64, idle: bool) -> Observation {
        Observation {
            capacity: CapacityStamp(capacity),
            idle,
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
            let Step::Await(awaiting) =
                rerouting.after(CapacityStamp(0), superseded(hop, generation))
            else {
                panic!("deferral {generation} is within budget");
            };
            rerouting = awaiting.resume();
        }
        let last = u64::from(REROUTING_BUDGET) + 1;
        let Step::Fail(Error::ReroutingExhausted { last: cause }) =
            rerouting.after(CapacityStamp(0), superseded(hop, last))
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
            Rerouting::start().after(CapacityStamp(0), Verdict::Accepted),
            Step::Complete
        ));
        let ambiguous = Error::DetachedSendAbandonedAfterClaim { peer: hop };
        assert!(matches!(
            Rerouting::start().after(
                CapacityStamp(0),
                Verdict::remote(hop, Some(1), Err(ambiguous))
            ),
            Step::Fail(Error::DetachedSendAbandonedAfterClaim { .. })
        ));
    }

    /// Law (S2): each trigger wakes on its own event and on a route move, and on nothing else.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_triggers_wake_on_their_events_and_route_moves_only() {
        let hop = Did::from(7_u32);
        let moved = through(Did::from(8_u32), 1, false);

        let link = awaiting(hop, 3, SendDeferral::cancelled(hop));
        assert!(!link.is_triggered(through(hop, 1, false), observed(9, true)));
        assert!(link.is_triggered(through(hop, 1, true), observed(3, false)));
        assert!(link.is_triggered(moved, observed(3, false)));
        assert!(link.is_triggered(LinkRoute::Local, observed(3, false)));

        let capacity = awaiting(
            hop,
            3,
            deferral(Error::OutboundTransferAdmissionTimeout {
                peer: hop,
                timeout_ms: 1,
            }),
        );
        assert!(!capacity.is_triggered(through(hop, 1, true), observed(3, true)));
        assert!(capacity.is_triggered(through(hop, 1, true), observed(4, false)));
        assert!(capacity.is_triggered(moved, observed(3, false)));

        let drain = awaiting(
            hop,
            3,
            deferral(Error::DataChannelSendQueueTimeout {
                peer: hop,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            }),
        );
        assert!(!drain.is_triggered(through(hop, 1, true), observed(9, false)));
        assert!(drain.is_triggered(through(hop, 1, true), observed(3, true)));
        assert!(drain.is_triggered(through(hop, 2, true), observed(3, false)));
        assert!(drain.is_triggered(moved, observed(3, false)));
    }
}
