//! The carrier of the rerouting model: one placement's automaton composed with the target
//! hop's production lifecycle registry, its readiness, the route preference, and local
//! capacity.
//!
//! The automaton's transitions are the production [`Rerouting::after`] and
//! [`Awaiting::is_triggered`]; the carrier stores their state as `Copy` projections (the
//! production `Awaiting` holds an `Error`, which is neither `Clone` nor `Hash`) and rebuilds the
//! production values at every step, so each step runs production code on production values.

use super::super::Awaiting;
use super::super::CapacityStamp;
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
    /// Capacity admission timed out: `OutboundTransferAdmissionTimeout`.
    AdmissionTimeout,
    /// The backend queue did not accept in time: `DataChannelSendQueueTimeout`.
    QueueTimeout,
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
}

/// Every ambiguity, in action order.
pub(super) const AMBIGUITIES: [Ambiguity; 5] = [
    Ambiguity::CompletionTimeout,
    Ambiguity::DeliveryTimeout,
    Ambiguity::CleanupTimeout,
    Ambiguity::PublishedAfterCancel,
    Ambiguity::NotDelivered,
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
        /// The deferrals the error reports.
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
        /// The target's sendable generation at binding; `None` for the alternate or when
        /// the target had none.
        generation: Option<u64>,
        /// Capacity epoch read before the send.
        stamp: u64,
    },
    /// `Waiting`: the fields of the production `Awaiting`.
    Waiting {
        /// `Awaiting::deferrals`.
        deferrals: u8,
        /// `Awaiting::hop`.
        hop: Hop,
        /// `Awaiting::stamp`.
        stamp: u64,
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
    /// Whether the next resolution meets exhausted capacity.
    pub(super) congested: bool,
    /// The capacity-release epoch.
    pub(super) capacity: u64,
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
    /// Env: the in-flight send is accepted, then fails ambiguously.
    AcceptThenFail(Ambiguity),
    /// Protocol (weakly fair): the pending generation becomes ready and is admitted.
    Admit,
    /// Protocol (weakly fair): the admitted generation recovers readiness.
    Recover,
    /// Protocol (weakly fair): the dead generation's close retires it.
    Close,
    /// Protocol (weakly fair): an admitted transfer releases capacity.
    Release,
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
            capacity: 0,
            phase: Phase::Compute { deferrals: 0 },
            churn: self.churn,
            effects: 0,
            sends: 0,
            last_refusal: None,
            stale_retry: false,
            reached_replacement: false,
            woke_on_capacity: false,
            woke_on_route: false,
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

    /// The production `LinkRoute` of the current route.
    pub(super) fn link_route(&self) -> LinkRoute {
        match self.route() {
            None => LinkRoute::Local,
            Some(hop) => LinkRoute::Remote {
                hop: hop.did(),
                usable: self.usable(hop),
            },
        }
    }
}

/// The production deferral `refusal` of a send to `hop` classifies into.
///
/// Panics when the classification no longer defers it: the model's refusals are the
/// pre-acceptance outcomes `send_class` proves deferrable.
pub(super) fn deferral(hop: Hop, refusal: Refusal) -> SendDeferral {
    match Verdict::remote(hop.did(), refusal.outcome(hop.did(), 0)) {
        Verdict::Deferred { cause, .. } => cause,
        verdict => panic!("{refusal:?} must defer, classified {verdict:?}"),
    }
}

/// Rebuild the production `Awaiting` of a waiting phase.
pub(super) fn awaiting(deferrals: u8, hop: Hop, stamp: u64, cause: Refusal) -> Awaiting {
    Awaiting {
        deferrals,
        hop: hop.did(),
        stamp: CapacityStamp(stamp),
        cause: deferral(hop, cause),
    }
}

/// Run the production `δ` from `deferrals` spent on a send to `hop` stamped `stamp`, and
/// project the step back into the carrier; `refusal` is the refusal the verdict was built
/// from, if any.
pub(super) fn transition(
    deferrals: u8,
    hop: Hop,
    stamp: u64,
    verdict: Verdict,
    refusal: Option<Refusal>,
) -> Phase {
    let deferred = match &verdict {
        Verdict::Deferred { cause, .. } => Some(cause.to_string()),
        Verdict::Accepted | Verdict::Failed(_) => None,
    };
    match (Rerouting { deferrals }).after(CapacityStamp(stamp), verdict) {
        Step::Complete => Phase::Done(Outcome::Accepted),
        Step::Await(Awaiting {
            deferrals, stamp, ..
        }) => match refusal {
            Some(cause) => Phase::Waiting {
                deferrals,
                hop,
                stamp: stamp.0,
                cause,
            },
            None => Phase::Done(Outcome::Unexpected),
        },
        Step::Fail(Error::ReroutingExhausted { deferrals, last }) => {
            Phase::Done(Outcome::Exhausted {
                deferrals,
                last_matches: deferred == Some(last.to_string()),
            })
        }
        Step::Fail(error) => Phase::Done(match error.send_class() {
            SendClass::Ambiguous => Outcome::Ambiguous,
            SendClass::Deferrable(_) | SendClass::Fatal => Outcome::Unexpected,
        }),
    }
}
