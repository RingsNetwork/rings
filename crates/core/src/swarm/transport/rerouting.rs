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
//! Compute(topology) ──route──▶ Local ───────────────────────────────▶ Done(local result)
//!        │
//!        └──▶ Remote(next) ──▶ Send(hop, generation) ──▶ Accepted ───▶ Done(Ok)
//!                                    │               ├─▶ Fatal(e)  ───▶ Done(Err e)
//!                                    │               └─▶ Deferred(d)
//!                                    │                      │ deferrals ≤ B
//!                                    │                      ▼
//!                                    │      Waiting(hop, stamp, d) ──Triggered──▶ Compute
//!                                    │                      │ deferrals > B
//!                                    │                      ▼
//!                                    └────────────▶ Exhausted(last = d)
//! Env ≜ Replace(g) ∨ Glare(g) ∨ Retire(g) ∨ TopologyChange ∨ Ready(g) ∨ Release(capacity)
//! ```
//!
//! `Compute` and `Send` are the effects of the caller (`storage::rerouted`); the verdict of a
//! send is classified by [`Verdict::remote`] through the total `Error::send_class`, whose module
//! proves every `Deferrable` class pre-acceptance. [`Rerouting::after`] is `δ` on verdicts and
//! [`Awaiting::is_triggered`] the guard of `Waiting → Compute`; both are pure.
//!
//! # Laws
//!
//! ```text
//! (S1) No duplicate effect.
//!      Pre : every Deferred(d) carries a SendDeferral, built only from a detached Cancelled or
//!            an error with send_class = Deferrable (type invariant of SendDeferral).
//!      Law : Effects(op, placement) ≤ 1.
//!      Proof: an attempt is retried only from Deferred(d) (δ has no other edge to Waiting);
//!            PreAcceptance(d) (send_class lemmas) ⇒ the attempt had no remote effect; a
//!            Local attempt and an Accepted or Fatal verdict end the automaton. So at most
//!            one attempt per placement can have an effect, append, tombstone and compact
//!            included.
//! (S2) Fresh hop.
//!      Law : every Compute after Waiting reads topology after the event that satisfied
//!            is_triggered.
//!      Proof: the shell leaves Waiting only when is_triggered(route, capacity) holds for a
//!            route recomputed after its listeners were registered, and the next Compute
//!            reads topology again; no hop survives a deferral (Awaiting keeps the old hop
//!            only to compare against).
//! (S3) Bounded.
//!      Inv : deferrals ≤ REROUTING_BUDGET in every Rerouting and Awaiting.
//!      Law : sends(op, placement) ≤ REROUTING_BUDGET + 1, and exhaustion returns
//!            Error::ReroutingExhausted { deferrals, last } with the last deferral's cause.
//! (L1) Liveness.
//!      Fairness: WF(Ready(g)) for every registered generation g of a hop that topology routes
//!            to, and WF(Release) while an admitted transfer holds capacity; every Env step
//!            advances an epoch the shell listens to (topology commit, link transition,
//!            capacity release).
//!      Law : if Env stops while deferrals ≤ REROUTING_BUDGET − QUIESCENT_DEFERRALS, then
//!            ◇ Done(Ok): a ready responsible hop is reached within the budget.
//!      Proof: after Env stops, at most QUIESCENT_DEFERRALS refusals remain (its derivation);
//!            each is followed by an event its trigger names (WF(Ready), WF(Release), or the
//!            close of a dead generation moving the route), which satisfies is_triggered, and
//!            a send to a usable hop with capacity is accepted. The model check decides this
//!            on every fair trace and finds a counterexample at QUIESCENT_DEFERRALS − 1.
//! ```
//!
//! The model check `test_model` enumerates every bounded trace of this automaton
//! composed with the production lifecycle registry and asserts S1–S3 on all of them and L1 on
//! the fair ones.

use event_listener::EventListener;
use futures::future::select;
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
/// finds a fair trace with exactly this many).
///
/// ```text
/// 1. the in-flight send's generation was superseded or died       (Superseded, Missing, …)
/// 2. a refusal on the current route, e.g. exhausted capacity      (AdmissionTimeout, …)
/// 3. the replacement generation is admitted (WF readiness), the
///    route moves to it, and that send meets the same condition
/// ```
pub(crate) const QUIESCENT_DEFERRALS: u8 = 3;

/// Deferrals one generation replacement racing the operation causes: the death of the bound
/// generation, and the admission of its replacement that moves the route mid-send.
pub(crate) const REPLACEMENT_DEFERRALS: u8 = 2;

/// Deferred sends one placement tolerates before `Exhausted`: what one generation replacement
/// racing the operation spends, plus what L1 needs once the environment stops.
pub(crate) const REROUTING_BUDGET: u8 = REPLACEMENT_DEFERRALS + QUIESCENT_DEFERRALS;

/// A reading of the capacity-release epoch, taken before a send.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub(crate) struct CapacityStamp(u64);

/// Where a placement's route leads now, as the wake guard reads it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum LinkRoute {
    /// This node holds the placement.
    Local,
    /// The first link hop toward the placement.
    Remote {
        /// Link peer the next send would be bound to.
        hop: Did,
        /// Whether `hop` has a sendable generation that can make progress now.
        usable: bool,
    },
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

    /// Classify a detached send bound to `hop`.
    ///
    /// `Ok(Cancelled)` is pre-acceptance by the single-publication lemma; an error defers iff
    /// its `send_class` is `Deferrable`.
    fn remote(hop: Did, result: Result<SendCompletionOutcome>) -> Self {
        match result {
            Ok(SendCompletionOutcome::Succeeded) => Self::Accepted,
            Ok(SendCompletionOutcome::Cancelled) => Self::Deferred {
                hop,
                cause: SendDeferral::cancelled(hop),
            },
            Err(error) => match SendDeferral::refused(error) {
                Ok(cause) => Self::Deferred { hop, cause },
                Err(error) => Self::Failed(error),
            },
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

/// `Waiting(hop, stamp, cause)`: a deferral within budget, awaiting its trigger.
///
/// Inv: `deferrals ≤ REROUTING_BUDGET`.
#[derive(Debug)]
pub(crate) struct Awaiting {
    /// Deferred sends so far, this one included.
    deferrals: u8,
    /// Link peer of the refused send.
    hop: Did,
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
            Verdict::Deferred { hop, cause } => {
                let deferrals = self.deferrals.saturating_add(1);
                if deferrals > REROUTING_BUDGET {
                    return Step::Fail(Error::ReroutingExhausted {
                        deferrals,
                        last: cause,
                    });
                }
                Step::Await(Awaiting {
                    deferrals,
                    hop,
                    stamp,
                    cause,
                })
            }
        }
    }
}

impl Awaiting {
    /// The guard of `Waiting → Compute` over a route recomputed now and the capacity epoch now.
    ///
    /// ```text
    /// Moved     ≜ route ≠ Remote(hop, _)
    /// Triggered ≜ Moved ∨ (trigger = LinkChange      ∧ route = Remote(hop, usable))
    ///                   ∨ (trigger = CapacityRelease ∧ capacity > stamp)
    /// ```
    ///
    /// Both disjuncts are state predicates, so a wait that starts after its event has already
    /// happened ends at once, and an unrelated event cannot spend budget.
    pub(crate) fn is_triggered(&self, route: LinkRoute, capacity: CapacityStamp) -> bool {
        let moved = !matches!(route, LinkRoute::Remote { hop, .. } if hop == self.hop);
        moved
            || match self.cause.trigger() {
                DeferralTrigger::LinkChange => {
                    matches!(route, LinkRoute::Remote { usable: true, .. })
                }
                DeferralTrigger::CapacityRelease => capacity > self.stamp,
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

    /// The link route toward `next` now: the hop `infer_next_hop` binds and its usability.
    pub(crate) fn link_route(&self, next: Did) -> Result<LinkRoute> {
        let hop = self.infer_next_hop(next, None)?;
        let usable = self
            .admitted_send_connection(hop)?
            .is_some_and(|admitted| admitted.connection().readiness().can_make_progress());
        Ok(LinkRoute::Remote { hop, usable })
    }

    /// Send `message` to `destination` through the hop `infer_next_hop` binds, detached, and
    /// classify the outcome.
    ///
    /// Unlike `PayloadSender::send_message`, a `Cancelled` completion is not reported as a
    /// success: it is the pre-acceptance deferral it is.
    pub(crate) async fn attempt_remote<T>(&self, message: T, destination: Did) -> Verdict
    where T: Serialize + Send {
        let hop = match self.infer_next_hop(destination, None) {
            Ok(hop) => hop,
            Err(error) => return Verdict::Failed(error),
        };
        let result = match self.signed_payload(message, hop, destination).await {
            Ok(payload) => self.send_payload_detached_with_outcome(payload).await,
            Err(error) => Err(error),
        };
        Verdict::remote(hop, result)
    }

    /// Register for the next event of every class that can trigger an [`Awaiting`].
    ///
    /// Pre: the caller evaluates [`Awaiting::is_triggered`] *after* this call and awaits
    /// [`ReroutingListeners::notified`] only when it is false, so no event is lost (`Law (Wake)`
    /// of `Epoch`).
    pub(crate) fn rerouting_listeners(&self) -> ReroutingListeners {
        ReroutingListeners {
            topology: self.dht.topology_epoch().listen(),
            link: self.connection_lifecycle.link_transitions().listen(),
            capacity: self.outbound_schedulers.capacity_releases().listen(),
        }
    }
}

/// One registration on the three epochs a waiting placement listens to.
pub(crate) struct ReroutingListeners {
    /// Next committed route change.
    topology: EventListener,
    /// Next admission, retirement, or readiness callback.
    link: EventListener,
    /// Next outbound capacity release.
    capacity: EventListener,
}

impl ReroutingListeners {
    /// Resolve at the first event of any class. No duration bounds this wait: it ends by an
    /// event (L1's fairness), or when the caller drops the operation.
    pub(crate) async fn notified(self) {
        let Self {
            topology,
            link,
            capacity,
        } = self;
        select(topology, select(link, capacity)).await;
    }
}

#[cfg(test)]
mod test_model;

#[cfg(test)]
mod tests {
    use super::Awaiting;
    use super::CapacityStamp;
    use super::LinkRoute;
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
            Err(Error::ConnectionAttemptSuperseded {
                peer: hop,
                generation,
            }),
        )
    }

    /// An awaiting state for `hop`, stamped `stamp`, deferred by `cause`.
    fn awaiting(hop: Did, stamp: u64, cause: SendDeferral) -> Awaiting {
        Awaiting {
            deferrals: 1,
            hop,
            stamp: CapacityStamp(stamp),
            cause,
        }
    }

    /// Law (S3): `REROUTING_BUDGET` deferrals await; the next fails with `ReroutingExhausted`
    /// carrying `REROUTING_BUDGET + 1` deferrals and the last cause.
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
        let Step::Fail(Error::ReroutingExhausted {
            deferrals,
            last: cause,
        }) = rerouting.after(CapacityStamp(0), superseded(hop, last))
        else {
            panic!("the deferral past the budget exhausts");
        };
        assert_eq!(deferrals, REROUTING_BUDGET + 1);
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
        let ambiguous = Error::DataChannelDeliveryTimeout {
            peer: hop,
            timeout_ms: 1,
            context: "test",
        };
        assert!(matches!(
            Rerouting::start().after(CapacityStamp(0), Verdict::remote(hop, Err(ambiguous))),
            Step::Fail(Error::DataChannelDeliveryTimeout { .. })
        ));
    }

    /// Law (S2): a link deferral wakes when the route moves or the hop is usable; a capacity
    /// deferral when the route moves or capacity was released.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_triggers_are_route_moves_usable_hops_and_capacity_releases() {
        let hop = Did::from(7_u32);
        let other = Did::from(8_u32);
        let unusable = LinkRoute::Remote { hop, usable: false };
        let usable = LinkRoute::Remote { hop, usable: true };
        let moved = LinkRoute::Remote {
            hop: other,
            usable: false,
        };

        let link = awaiting(hop, 3, SendDeferral::cancelled(hop));
        assert!(!link.is_triggered(unusable, CapacityStamp(9)));
        assert!(link.is_triggered(usable, CapacityStamp(3)));
        assert!(link.is_triggered(moved, CapacityStamp(3)));
        assert!(link.is_triggered(LinkRoute::Local, CapacityStamp(3)));

        let refused = Error::OutboundTransferAdmissionTimeout {
            peer: hop,
            timeout_ms: 1,
        };
        let capacity = match SendDeferral::refused(refused) {
            Ok(cause) => awaiting(hop, 3, cause),
            Err(error) => panic!("capacity exhaustion defers: {error}"),
        };
        assert!(!capacity.is_triggered(usable, CapacityStamp(3)));
        assert!(capacity.is_triggered(usable, CapacityStamp(4)));
        assert!(capacity.is_triggered(moved, CapacityStamp(3)));
    }
}
