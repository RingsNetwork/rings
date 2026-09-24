//! The carrier of the rerouting model: one placement's automaton composed with the target
//! hop's production lifecycle registry, its readiness, the route preference, and local
//! capacity.
//!
//! The automaton's transitions are the production [`Rerouting::after`] and
//! [`Awaiting::is_triggered`]; the carrier stores their state as `Copy` projections (the
//! production `Awaiting` holds an `Error`, which is neither `Clone` nor `Hash`) and rebuilds the
//! production values at every step, so each step runs production code on production values.

use super::super::Awaiting;
use super::super::LinkHop;
use super::super::LinkRoute;
use super::super::Rerouting;
use super::super::Step;
use super::super::Verdict;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::error::SendClass;
use crate::error::SendDeferral;
use crate::swarm::transport::delivery::SendCompletionOutcome;
use crate::swarm::transport::outbound::admits_now;
use crate::swarm::transport::outbound::CapacityScope;
use crate::swarm::transport::outbound::PeerProgress;
use crate::swarm::transport::outbound::TransferDemand;
use crate::swarm::transport::pending::ConnectionLifecycleRegistry;
use crate::swarm::transport::pending::LifecycleBounds;
use crate::swarm::transport::pending::PendingConnectionAttempt;

/// A link hop the route can name: the churning target, whose generations are the production
/// registry's, or the stable alternate, which is always usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Hop {
    /// The churning hop.
    Target,
    /// The stable hop.
    Alternate,
}

impl Hop {
    /// The hop's slot in per-hop arrays.
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Target => 0,
            Self::Alternate => 1,
        }
    }

    /// The hop's identity: the target is `1`, the alternate `2`.
    pub(super) fn did(self) -> Did {
        match self {
            Self::Target => Did::from(1_u32),
            Self::Alternate => Did::from(2_u32),
        }
    }
}

/// Which way the topology routes the placement when the target is admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Preference {
    /// Through the target, while it holds an admitted generation; otherwise the alternate.
    Target,
    /// Through the alternate.
    Alternate,
    /// This node owns the placement.
    Local,
}

/// A pre-acceptance outcome the send path produces (each a production value, see
/// [`Refusal::outcome`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Refusal {
    /// No sendable generation at preparation: `SwarmMissDidInTable`.
    Missing,
    /// The bound generation lost its slot: `ConnectionAttemptSuperseded`.
    Superseded,
    /// The claim failed: `Transport(SendPermitRevoked)`.
    PermitRevoked,
    /// The detached transfer completed `Cancelled`.
    Cancelled,
    /// The bound generation cannot make progress: `TransportNotReady`.
    NotReady,
    /// The hop's own capacity is full: `OutboundTransferCapacityExceeded` (`CapacityRelease`,
    /// peer scope).
    PeerFull,
    /// Shared capacity admission timed out: `OutboundTransferAdmissionTimeout`
    /// (`CapacityRelease`, global scope).
    AdmissionTimeout,
    /// The backend queue did not accept in time: `DataChannelSendQueueTimeout`
    /// (`ChannelDrain`).
    QueueTimeout,
}

impl Refusal {
    /// The model's own reading of the trigger, independent of `send_class`.
    pub(super) const fn trigger(self) -> Trigger {
        match self {
            Self::PeerFull => Trigger::PeerCapacity,
            Self::AdmissionTimeout => Trigger::GlobalCapacity,
            Self::QueueTimeout => Trigger::Drain,
            Self::Missing
            | Self::Superseded
            | Self::PermitRevoked
            | Self::Cancelled
            | Self::NotReady => Trigger::Link,
        }
    }
}

/// The event class a refusal waits for, as the model defines it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Trigger {
    /// A usable hop, or a route move.
    Link,
    /// A release by another transfer of the hop, or a route move.
    PeerCapacity,
    /// A release by any other transfer, or a route move.
    GlobalCapacity,
    /// The hop's channel drained or its generation changed, or a route move.
    Drain,
}

/// A post-acceptance failure the send path produces: every `Ambiguous` error a detached
/// send can return (see `error::send_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Ambiguity {
    /// `DataChannelSendCompletionTimeout`.
    CompletionTimeout,
    /// `DataChannelDeliveryTimeout`.
    DeliveryTimeout,
    /// `DetachedPayloadCleanupTimeout`.
    CleanupTimeout,
    /// `CancelledDetachedAdmissionPublishedSuccess`.
    PublishedAfterCancel,
    /// `Transport(MessageNotDelivered)`.
    NotDelivered,
    /// `DetachedSendAbandonedAfterClaim`: the worker stopped after the claim.
    AbandonedAfterClaim,
}

/// Every ambiguity, in action order.
pub(super) const AMBIGUITIES: [Ambiguity; 6] = [
    Ambiguity::CompletionTimeout,
    Ambiguity::DeliveryTimeout,
    Ambiguity::CleanupTimeout,
    Ambiguity::PublishedAfterCancel,
    Ambiguity::NotDelivered,
    Ambiguity::AbandonedAfterClaim,
];

impl Refusal {
    /// The production outcome of a detached send to `hop` bound to `generation`.
    pub(super) fn outcome(self, hop: Did, generation: u64) -> Result<SendCompletionOutcome> {
        Err(match self {
            Self::Missing => Error::SwarmMissDidInTable(hop),
            Self::Superseded => Error::ConnectionAttemptSuperseded {
                peer: hop,
                generation,
            },
            Self::PermitRevoked => {
                Error::Transport(rings_transport::error::Error::SendPermitRevoked)
            }
            Self::Cancelled => return Ok(SendCompletionOutcome::Cancelled),
            Self::NotReady => Error::TransportNotReady {
                state: rings_transport::core::transport::WebrtcConnectionState::Disconnected,
                data_channel_open: true,
            },
            Self::PeerFull => Error::OutboundTransferCapacityExceeded {
                peer: hop,
                capacity: 1,
            },
            Self::AdmissionTimeout => Error::OutboundTransferAdmissionTimeout {
                peer: hop,
                timeout_ms: 1,
            },
            Self::QueueTimeout => Error::DataChannelSendQueueTimeout {
                peer: hop,
                timeout_ms: 1,
                bytes: 1,
                context: "model",
            },
        })
    }
}

impl Ambiguity {
    /// The production error of a send to `hop` that the backend may have accepted.
    pub(super) fn error(self, hop: Did) -> Error {
        match self {
            Self::CompletionTimeout => Error::DataChannelSendCompletionTimeout {
                peer: hop,
                timeout_ms: 1,
                bytes: 1,
                context: "model",
            },
            Self::DeliveryTimeout => Error::DataChannelDeliveryTimeout {
                peer: hop,
                timeout_ms: 1,
                context: "model",
            },
            Self::CleanupTimeout => Error::DetachedPayloadCleanupTimeout {
                peer: hop,
                timeout_ms: 1,
            },
            Self::PublishedAfterCancel => Error::CancelledDetachedAdmissionPublishedSuccess,
            Self::NotDelivered => Error::Transport(
                rings_transport::error::Error::MessageNotDelivered("model".to_string()),
            ),
            Self::AbandonedAfterClaim => Error::DetachedSendAbandonedAfterClaim { peer: hop },
        }
    }
}

/// How the placement ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Outcome {
    /// Accepted or settled locally.
    Accepted,
    /// A post-acceptance failure surfaced unretried.
    Ambiguous,
    /// `ReroutingExhausted` with the cause of its last deferral.
    Exhausted {
        /// The deferrals spent, the exhausting one included.
        deferrals: u8,
        /// Whether its cause is the carrier's last recorded refusal.
        last_matches: bool,
    },
    /// Any other error: a model defect.
    Unexpected,
}

/// The automaton's phase, as `Copy` projections of the production state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Phase {
    /// `Compute` with `deferrals` spent.
    Compute {
        /// `Rerouting::deferrals`.
        deferrals: u8,
    },
    /// `Send(hop, generation)`: bound, not yet resolved.
    InFlight {
        /// `Rerouting::deferrals`.
        deferrals: u8,
        /// The bound hop.
        hop: Hop,
        /// The hop's sendable generation at binding (`LinkHop::generation`).
        generation: Option<u64>,
    },
    /// `Waiting`: the fields of the production `Awaiting`.
    Waiting {
        /// `Awaiting::deferrals`.
        deferrals: u8,
        /// `Awaiting::hop`.
        hop: Hop,
        /// `Awaiting::generation`.
        generation: Option<u64>,
        /// The hop's peer release epoch read after the refusal (`PeerStamp`).
        peer: u64,
        /// The refusal its cause was built from.
        cause: Refusal,
    },
    /// The placement ended.
    Done(Outcome),
}

/// The budgets of the environment: how much churn a trace may contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Churn {
    /// Reservations after the initial generation (so generations ≤ 1 + this).
    pub(super) reservations: u8,
    /// Glare replacements of a pending generation (each also spends a reservation).
    pub(super) glare: u8,
    /// Pending generations withdrawn before readiness.
    pub(super) withdrawals: u8,
    /// Deaths of an admitted generation (send-terminal, then closed).
    pub(super) deaths: u8,
    /// Readiness losses of the admitted generation.
    pub(super) disconnects: u8,
    /// Route-preference changes.
    pub(super) reroutes: u8,
    /// Capacity exhaustions.
    pub(super) congestions: u8,
    /// Other transfers entering a hop's channel.
    pub(super) jams: u8,
    /// A hop's own capacity filling up (its in-flight transfers hold every slot).
    pub(super) fills: u8,
    /// A large transfer queuing on the shared capacity.
    pub(super) queues: u8,
    /// Sends accepted and then failed ambiguously.
    pub(super) ambiguities: u8,
}

/// One state of the composed carrier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct State {
    /// The target's lifecycle registry (production).
    pub(super) registry: ConnectionLifecycleRegistry,
    /// Whether the target's admitted generation can make progress.
    pub(super) ready: bool,
    /// The route preference.
    pub(super) preference: Preference,
    /// Whether shared (global) capacity is exhausted.
    pub(super) congested: bool,
    /// Whether a waiter is queued on the shared capacity, so unqueued admission is refused.
    pub(super) queued: bool,
    /// Other transfers holding each hop's channel and capacity (by `Hop::index`).
    pub(super) jam: [u8; 2],
    /// Whether each hop's own capacity is full. Inv: `full[h] ⇒ jam[h] > 0`.
    pub(super) full: [bool; 2],
    /// Each hop's peer release epoch.
    pub(super) released: [u64; 2],
    /// The automaton.
    pub(super) phase: Phase,
    /// Remaining environment budget.
    pub(super) churn: Churn,
    /// History: effects of the operation at this placement.
    pub(super) effects: u8,
    /// History: sends bound.
    pub(super) sends: u8,
    /// History: the last refusal.
    pub(super) last_refusal: Option<Refusal>,
    /// History: a retry whose trigger the model's own freshness condition rejects.
    pub(super) stale_retry: bool,
    /// History: a retry reached a generation newer than the one first bound.
    pub(super) reached_replacement: bool,
    /// History: a wait ended by a capacity release.
    pub(super) woke_on_capacity: bool,
    /// History: a wait ended by a route change.
    pub(super) woke_on_route: bool,
    /// History: a wait ended by a channel drain or a generation change.
    pub(super) woke_on_drain: bool,
}

/// One step of the carrier.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Action {
    /// Env: reserve a generation for the absent target.
    Dial,
    /// Env: glare: withdraw the pending generation and reserve the peer's offer instead.
    Glare,
    /// Env: withdraw the pending generation before it is ready (retire-before-ready).
    Withdraw,
    /// Env: the admitted generation's link dies (`mark_send_terminal`).
    Die,
    /// Env: the admitted generation loses readiness (`Disconnected`).
    Disconnect,
    /// Env: the topology changes the route preference.
    Reroute(Preference),
    /// Env: local capacity is exhausted for the next resolution.
    Congest,
    /// Env: another transfer enters the hop's channel.
    Jam(Hop),
    /// Env: the hop's own capacity fills up.
    FillPeer(Hop),
    /// Env: a large transfer queues on the shared capacity.
    Enqueue,
    /// Env: the in-flight send is accepted, then fails ambiguously.
    AcceptThenFail(Ambiguity),
    /// Protocol (weakly fair): the pending generation becomes ready and is admitted.
    Admit,
    /// Protocol (weakly fair): the admitted generation recovers readiness.
    Recover,
    /// Protocol (weakly fair): the dead generation's close retires it.
    Close,
    /// Protocol (weakly fair): another peer's transfer releases enough to clear shared
    /// congestion.
    Release,
    /// Protocol (weakly fair): another transfer of the hop completes, releasing its peer and
    /// global capacity (it clears the hop's full capacity, not shared congestion).
    Drain(Hop),
    /// Protocol (weakly fair): the queued waiter is admitted or cancelled (a departure).
    Dequeue,
    /// Protocol: compute the route and bind a send, or settle locally.
    Send,
    /// Protocol: the in-flight send resolves with the backend's acceptance.
    Accept,
    /// Protocol: the in-flight send resolves with a pre-acceptance refusal.
    Refuse(Refusal),
    /// Protocol: the wait's trigger holds; compute again.
    Wake,
}

impl Action {
    /// Whether the environment takes this step.
    pub(super) const fn is_environmental(&self) -> bool {
        matches!(
            self,
            Self::Dial
                | Self::Glare
                | Self::Withdraw
                | Self::Die
                | Self::Disconnect
                | Self::Reroute(_)
                | Self::Congest
                | Self::Jam(_)
                | Self::FillPeer(_)
                | Self::Enqueue
                | Self::AcceptThenFail(_)
        )
    }
}

/// A deliberate defect of the automaton, to show a law is not vacuous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mutation {
    /// The production automaton.
    Faithful,
    /// An ambiguous failure is deferred as if it were pre-acceptance (breaks S1).
    RetryAmbiguous,
    /// A wait ends although its trigger does not hold (breaks S2).
    WakeUntriggered,
}

/// The model: its initial churn budget and the automaton under check.
#[derive(Debug)]
pub(super) struct Model {
    /// The churn a trace may contain.
    pub(super) churn: Churn,
    /// The automaton: production, or a mutant.
    pub(super) mutation: Mutation,
}

/// Bounds of the modeled registry: one peer is all it holds.
const MODEL_BOUNDS: LifecycleBounds = LifecycleBounds::new(1, 1);

impl Model {
    /// `Init`: the target admitted and ready under generation 1, routed through, no work
    /// done.
    pub(super) fn init(&self) -> State {
        let mut registry = ConnectionLifecycleRegistry::new(MODEL_BOUNDS);
        let first = registry
            .reserve(Hop::Target.did(), 0)
            .expect("an empty registry admits the target");
        admit(&mut registry, first);
        State {
            registry,
            ready: true,
            preference: Preference::Target,
            congested: false,
            queued: false,
            jam: [0; 2],
            full: [false; 2],
            released: [0; 2],
            phase: Phase::Compute { deferrals: 0 },
            churn: self.churn,
            effects: 0,
            sends: 0,
            last_refusal: None,
            stale_retry: false,
            reached_replacement: false,
            woke_on_capacity: false,
            woke_on_route: false,
            woke_on_drain: false,
        }
    }
}

/// `Pending(attempt) → Admitting → Active`, the production admission of a ready generation.
pub(super) fn admit(registry: &mut ConnectionLifecycleRegistry, attempt: PendingConnectionAttempt) {
    assert!(
        registry.begin_admission(attempt),
        "a pending generation begins admission"
    );
    registry
        .admitting_connection(attempt)
        .expect("an admitting generation activates")
        .activate();
}

impl State {
    /// The hop the topology routes the placement through now, or `None` when local.
    ///
    /// Topology references the target iff it holds an admitted generation (sendable or dead
    /// and not yet closed), `TopologyReferencesOnlyAdmitted` of the rejoin model.
    pub(super) fn route(&self) -> Option<Hop> {
        match self.preference {
            Preference::Local => None,
            Preference::Target if self.registry.active_attempt(Hop::Target.did()).is_some() => {
                Some(Hop::Target)
            }
            Preference::Target | Preference::Alternate => Some(Hop::Alternate),
        }
    }

    /// Whether `hop` has a sendable generation that can make progress.
    pub(super) fn usable(&self, hop: Hop) -> bool {
        match hop {
            Hop::Target => {
                self.registry.sendable_attempt(Hop::Target.did()).is_some() && self.ready
            }
            Hop::Alternate => true,
        }
    }

    /// The sendable generation of `hop`: the target's registry generation, `0` for the
    /// stable alternate.
    pub(super) fn generation(&self, hop: Hop) -> Option<u64> {
        match hop {
            Hop::Target => self
                .registry
                .sendable_attempt(Hop::Target.did())
                .map(|attempt| attempt.generation()),
            Hop::Alternate => Some(0),
        }
    }

    /// The production `LinkRoute` of the current route.
    pub(super) fn link_route(&self) -> LinkRoute {
        match self.route() {
            None => LinkRoute::Local,
            Some(hop) => LinkRoute::Remote(LinkHop {
                hop: hop.did(),
                generation: self.generation(hop),
                usable: self.usable(hop),
            }),
        }
    }

    /// `Room(hop, demand)`: the production combinator `admits_now` over the model's scopes.
    /// Model demands exceed every fixed reservation, so only the shared step, through an empty
    /// queue, admits them.
    pub(super) fn has_room(&self, hop: Hop) -> bool {
        admits_now(!self.queued, |scope| match scope {
            CapacityScope::FixedReservation => false,
            CapacityScope::Shared => !self.full[hop.index()] && !self.congested,
        })
    }

    /// The production `PeerProgress` of `hop` against a peer stamp `peer`.
    pub(super) fn progress(&self, hop: Hop, peer: u64) -> PeerProgress {
        PeerProgress {
            released: self.released[hop.index()] > peer,
            idle: self.jam[hop.index()] == 0,
        }
    }
}

/// The production deferral `refusal` of a send to `hop` classifies into.
///
/// Panics when the classification no longer defers it: the model's refusals are the
/// pre-acceptance outcomes `send_class` proves deferrable.
pub(super) fn deferral(hop: Hop, refusal: Refusal) -> SendDeferral {
    match Verdict::remote(
        hop.did(),
        None,
        TransferDemand::for_test(),
        refusal.outcome(hop.did(), 0),
    ) {
        Verdict::Deferred { cause, .. } => cause,
        verdict => panic!("{refusal:?} must defer, classified {verdict:?}"),
    }
}

/// Rebuild the production `Awaiting` of a waiting phase.
pub(super) fn awaiting(
    deferrals: u8,
    hop: Hop,
    generation: Option<u64>,
    cause: Refusal,
) -> Awaiting {
    Awaiting {
        deferrals,
        hop: hop.did(),
        generation,
        demand: TransferDemand::for_test(),
        cause: deferral(hop, cause),
    }
}

/// Where a resolved send stood: its hop, and the hop's peer release epoch read after its
/// resolution.
#[derive(Clone, Copy)]
pub(super) struct Resolved {
    /// The bound hop.
    pub(super) hop: Hop,
    /// The hop's peer release epoch read after the resolution.
    pub(super) peer: u64,
}

/// Run the production `δ` from `deferrals` spent on the resolved send, and project the step
/// back into the carrier; `refusal` is the refusal the verdict was built from, if any.
pub(super) fn transition(
    deferrals: u8,
    resolved: Resolved,
    verdict: Verdict,
    refusal: Option<Refusal>,
) -> Phase {
    let deferred = match &verdict {
        Verdict::Deferred { cause, .. } => Some(cause.to_string()),
        Verdict::Accepted | Verdict::Failed(_) => None,
    };
    match (Rerouting { deferrals }).after(verdict) {
        Step::Complete => Phase::Done(Outcome::Accepted),
        Step::Await(Awaiting {
            deferrals,
            generation,
            ..
        }) => match refusal {
            Some(cause) => Phase::Waiting {
                deferrals,
                hop: resolved.hop,
                generation,
                peer: resolved.peer,
                cause,
            },
            None => Phase::Done(Outcome::Unexpected),
        },
        Step::Fail(Error::ReroutingExhausted { last }) => Phase::Done(Outcome::Exhausted {
            deferrals: deferrals.saturating_add(1),
            last_matches: deferred == Some(last.to_string()),
        }),
        Step::Fail(error) => Phase::Done(match error.send_class() {
            SendClass::Ambiguous => Outcome::Ambiguous,
            SendClass::Deferrable(_) | SendClass::Fatal => Outcome::Unexpected,
        }),
    }
}
