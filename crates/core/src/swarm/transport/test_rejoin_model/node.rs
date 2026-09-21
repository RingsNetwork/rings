//! One peer of the composed model: the production topology state and the
//! production lifecycle registry, advanced by the shell interpretation that
//! joins them.
//!
//! Nothing here re-implements a production state machine. Every topology
//! change is `topology::step`, every lifecycle change is a
//! [`ConnectionLifecycleRegistry`] method, the bounded candidate budget is
//! the production [`ConnectionPlan`], and the removal flavours
//! are the production [`DhtPeerRemoval`]. What this module owns is the
//! *composition* that production performs inside `SwarmTransport` under the
//! lifecycle boundary, written as pure functions `NodeState × Input →
//! NodeState × [Effect]`:
//!
//! ```text
//! event(g) ──▶ [registry: does `g` own the slot?] ── no ──▶ inert
//!                             │ yes
//!                             ▼
//!           [registry transition] ⨯ [topology::step] (one atomic step)
//!                             │
//!                             ▼
//!                  [effects: messages, offers]
//! ```
//!
//! Each function names the production path it interprets, so a reviewer can
//! hold the two side by side.

use std::collections::BTreeSet;

use super::overlay::Overlay;
use super::overlay::ShellMutation;
use crate::dht::topology::stabilization_connection_budget;
use crate::dht::topology::step;
use crate::dht::topology::successor_head;
use crate::dht::topology::successors;
use crate::dht::topology::ConnectionPlan;
use crate::dht::topology::ConnectionStep;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::Did;
use crate::dht::TopoInfo;
use crate::swarm::transport::connection::DhtPeerRemoval;
use crate::swarm::transport::pending::ActiveConnectionSet;
use crate::swarm::transport::pending::ConnectionLifecycleRegistry;
use crate::swarm::transport::pending::LifecycleBounds;
use crate::swarm::transport::pending::PendingConnectionAttempt;
use crate::swarm::transport::pending::RetirementOutcome;

/// Reservation timestamp of every modeled attempt.
///
/// The registry stores it only to age unadmitted attempts in `expire`, which
/// this model never calls (handshake expiry is witnessed by
/// `pending::test_lifecycle_model`); a constant keeps time out of the carrier.
const RESERVED_AT_MS: i64 = 0;

/// Monotonic time supplied to `TopologyEvent::Admit`.
///
/// It paces only a deferred finger proof, and the model admits none, so the
/// value is unobservable here.
const ADMITTED_AT_MS: u64 = 0;

/// A local event of one connection generation, delivered at any later time.
///
/// An event is addressed to generation `g`, not to a peer: the receiving
/// shell must decide whether `g` still owns the peer's slot. That decision is
/// the exact-generation guard under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum LifecycleEvent {
    /// The data channel of this generation opened (`on_data_channel_open`).
    ChannelOpened(PendingConnectionAttempt),
    /// A send on this generation failed terminally (`mark_send_terminal`).
    SendTerminal(PendingConnectionAttempt),
    /// The unavailable-peer sweep selected this send-terminal generation
    /// (`disconnect_unavailable`).
    RetireUnavailable(PendingConnectionAttempt),
    /// The transport of this generation reached `Failed`/`Closed`
    /// (`leave_dht_attempt`), or its offer was refused and the generation
    /// expired.
    Closed(PendingConnectionAttempt),
}

impl LifecycleEvent {
    /// The generation this event was bound to when its connection was made.
    pub(super) const fn attempt(self) -> PendingConnectionAttempt {
        match self {
            Self::ChannelOpened(attempt)
            | Self::SendTerminal(attempt)
            | Self::RetireUnavailable(attempt)
            | Self::Closed(attempt) => attempt,
        }
    }

    /// The same event re-addressed to `attempt`: the functorial action of a
    /// generation substitution on events, used only by the mutated shell.
    const fn readdressed(self, attempt: PendingConnectionAttempt) -> Self {
        match self {
            Self::ChannelOpened(_) => Self::ChannelOpened(attempt),
            Self::SendTerminal(_) => Self::SendTerminal(attempt),
            Self::RetireUnavailable(_) => Self::RetireUnavailable(attempt),
            Self::Closed(_) => Self::Closed(attempt),
        }
    }
}

/// A stabilization-protocol message carried by an admitted connection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum ProtocolMessage {
    /// `QueryForTopoInfoSend` for stabilization, with its correlation token.
    TopologyQuery {
        /// Token the report must echo.
        request_id: uuid::Uuid,
    },
    /// `QueryForTopoInfoReport`: the reporter's successor list and predecessor.
    TopologyReport {
        /// Token echoed from the query.
        request_id: uuid::Uuid,
        /// Reporter's successor sequence at answer time.
        successors: Vec<Did>,
        /// Reporter's predecessor at answer time.
        predecessor: Option<Did>,
    },
    /// `NotifyPredecessorSend`: the sender proposes itself as predecessor.
    NotifyPredecessor,
}

/// An effect requested by a node transition, as data.
#[derive(Debug)]
pub(super) enum Effect {
    /// Send `message` on the sendable generation `under`.
    Message {
        /// Local generation the send was admitted under.
        under: PendingConnectionAttempt,
        /// Payload.
        message: ProtocolMessage,
    },
    /// Signal an offer for the freshly reserved generation `offered`.
    Offer {
        /// Local generation reserved for the offered peer.
        offered: PendingConnectionAttempt,
    },
}

/// The witness that an unavailable head was not replaced by the sendable
/// admitted successors: the history variable of the head-replacement law.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct UnreplacedHead {
    /// The head that was retired as unavailable.
    pub(super) removed: Did,
}

/// One live peer: production topology × production lifecycle registry, plus
/// the local events raised but not yet delivered to its shell, and the
/// history variable of the head-replacement law.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct NodeState {
    /// Production pure topology state.
    pub(super) topology: TopologyState,
    /// Production connection lifecycle registry.
    pub(super) lifecycles: ConnectionLifecycleRegistry,
    /// Local events raised but not yet delivered.
    pub(super) events: BTreeSet<LifecycleEvent>,
    /// Set when an unavailable head's retirement left a successor sequence
    /// other than the sendable admitted successors; never set by the
    /// faithful shell.
    pub(super) unreplaced_head: Option<UnreplacedHead>,
}

/// Result of one node transition: `NodeState × [Effect]`.
pub(super) struct NodeStep {
    /// Next node state.
    pub(super) node: NodeState,
    /// Effects for the environment to dispatch.
    pub(super) effects: Vec<Effect>,
}

impl NodeState {
    /// A freshly started peer: empty topology, empty registry.
    ///
    /// A restart is a new process, so generations restart too; no event of
    /// the previous process survives to collide with them. The registry
    /// bounds equal the ring size, so its capacity verdicts are unreachable
    /// and `reserve` can fail only with `AlreadyConnected`.
    pub(super) fn started(local: Did, overlay: &Overlay) -> Self {
        let peers = overlay.ring().len();
        Self {
            topology: TopologyState::new(local, Vec::new(), None, vec![
                None;
                overlay.finger_slots()
            ]),
            lifecycles: ConnectionLifecycleRegistry::new(LifecycleBounds::new(peers, peers)),
            events: BTreeSet::new(),
            unreplaced_head: None,
        }
    }

    /// `Owns(g)`: generation `g` is the one recorded for its peer, in any phase.
    ///
    /// `¬Owns(g)` is the definition of a *retired* generation: every event
    /// bound to such a `g` must be inert.
    pub(super) fn owns(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles
            .state(attempt.peer())
            .is_some_and(|lifecycle| lifecycle.attempt() == attempt)
    }

    /// `Holds(g)`: `g` owns its slot but is not yet admitted, so production
    /// parks its inbound frames in the pre-admission hold
    /// (`InboundGate::Unadmitted`).
    pub(super) fn holds_before_admission(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles.unadmitted_attempt(attempt.peer()) == Some(attempt)
    }

    /// `Admits(g)`: `g` is the admitted generation of its peer, the inbound
    /// gate of `InboundProcessor::pending_connection_gate`
    /// (`InboundGate::Admitted`).
    fn admits(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles.active_attempt(attempt.peer()) == Some(attempt)
    }

    /// `Sendable(g)`: `g` is admitted and not send-terminal, the production
    /// routability of a peer up to transport readiness.
    fn sendable(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles.sendable_attempt(attempt.peer()) == Some(attempt)
    }

    /// `Isolated`: no lifecycle record exists for any ring identity, so only
    /// a bootstrap dial can reconnect this peer.
    pub(super) fn is_isolated(&self, overlay: &Overlay) -> bool {
        overlay
            .ring()
            .iter()
            .all(|peer| !self.lifecycles.contains(*peer))
    }

    /// The observable of the retired-generation law: everything a retired
    /// generation's event is forbidden to change.
    pub(super) fn retirement_observable(&self) -> (&TopologyState, &ConnectionLifecycleRegistry) {
        (&self.topology, &self.lifecycles)
    }

    /// Apply one production topology transition and return its actions.
    fn advance(&mut self, event: TopologyEvent, overlay: &Overlay) -> Vec<TopologyAction> {
        let next = step(&self.topology, event, overlay.successor_capacity());
        self.topology = next.state;
        next.actions
    }

    /// `message` addressed to `peer`, iff a sendable generation exists
    /// (`sendable_attempt`, the production send gate).
    fn message_to(&self, peer: Did, message: ProtocolMessage) -> Option<Effect> {
        self.lifecycles
            .sendable_attempt(peer)
            .map(|under| Effect::Message { under, message })
    }

    /// Interpret topology actions as effects.
    ///
    /// `QuerySuccessorTopology` whose send gate is closed cancels its token,
    /// as `Stabilizer::correct_stabilize` does on a send error. Successor-list
    /// sync, connect lookups, and placement repair are outside this model.
    fn interpret(&mut self, actions: Vec<TopologyAction>, overlay: &Overlay) -> Vec<Effect> {
        let mut effects = Vec::new();
        for action in actions {
            match action {
                TopologyAction::QuerySuccessorTopology {
                    successor,
                    request_id,
                } => {
                    match self.message_to(successor, ProtocolMessage::TopologyQuery { request_id })
                    {
                        Some(effect) => effects.push(effect),
                        None => {
                            self.advance(TopologyEvent::CancelStabilize { request_id }, overlay);
                        }
                    }
                }
                TopologyAction::Notify(successor) => {
                    effects.extend(self.message_to(successor, ProtocolMessage::NotifyPredecessor));
                }
                TopologyAction::FindSuccessorForConnect { .. }
                | TopologyAction::FindSuccessorForFix { .. }
                | TopologyAction::QuerySuccessorList(_)
                | TopologyAction::SuccessorHeadChanged(_) => {}
            }
        }
        effects
    }

    /// `SwarmTransport::connect`: reserve a generation and signal its offer.
    ///
    /// A peer that already owns a record yields no effect: `AlreadyConnected`
    /// is success for `connect_dht_peer`, and it is the only failure the
    /// registry can report under the modeled bounds (see [`Self::started`]).
    fn offer_to(&mut self, peer: Did) -> Option<Effect> {
        self.lifecycles
            .reserve(peer, RESERVED_AT_MS)
            .ok()
            .map(|offered| Effect::Offer { offered })
    }

    /// Bootstrap dial of an isolated peer.
    pub(super) fn dial(mut self, peer: Did) -> NodeStep {
        let effects = self.offer_to(peer).into_iter().collect();
        NodeStep {
            node: self,
            effects,
        }
    }

    /// `reconcile_incoming_offer_peer` then `create_connection_answer`.
    ///
    /// ```text
    /// [admitted record?] ── sendable ──▶ refuse (AlreadyConnected)
    ///        │ send-terminal: retire it as unavailable
    ///        ▼
    /// [unadmitted record?] ── Pending ∧ local > offerer ──▶ abandon own offer
    ///        │ otherwise ─────────────────────────────────▶ refuse
    ///        ▼
    /// reserve(offerer) ──▶ answer under the new generation
    /// ```
    ///
    /// Production abandons its own pending offer only while that raw
    /// connection is still `New`, i.e. unanswered. In the model a pending
    /// generation that was answered is paired with a link whose far end the
    /// offerer still owns, so the offerer cannot hold a second, standing
    /// offer for it: at delivery the own offer is unanswered whenever it is
    /// alive. The one divergence is a pending generation whose link has
    /// already died: production refuses until its close is observed, the
    /// model abandons it at once and later consumes that close inertly, a
    /// superset schedule with the same end state. Returns the answering
    /// generation, or `None` when the offer is refused.
    pub(super) fn answer_offer(
        mut self,
        offerer: Did,
        overlay: &Overlay,
    ) -> (Self, Option<PendingConnectionAttempt>) {
        if let Some(admitted) = self.lifecycles.active_attempt(offerer) {
            if self.lifecycles.sendable_attempt(offerer).is_some() {
                return (self, None);
            }
            self.retire(admitted, DhtPeerRemoval::Unavailable, overlay);
        }
        if let Some(unadmitted) = self.lifecycles.unadmitted_attempt(offerer) {
            let abandons_own_offer = self.lifecycles.pending_attempt(offerer) == Some(unadmitted)
                && self.topology.local > offerer;
            if !abandons_own_offer {
                return (self, None);
            }
            self.lifecycles.remove_unadmitted(unadmitted);
        }
        let answered = self.lifecycles.reserve(offerer, RESERVED_AT_MS).ok();
        (self, answered)
    }

    /// Deliver one local event.
    ///
    /// Under [`ShellMutation::CallbackIgnoresGeneration`] the event is first
    /// re-addressed to whatever generation currently owns the peer, which is
    /// the defect the exact-generation guard exists to exclude. `Closed` of
    /// a generation that owns nothing is inert here; production's
    /// `remove_retired_attempt_topology` fallback would then remove a peer
    /// no admitted generation backs, which `TopologyReferencesOnlyAdmitted`
    /// proves is never referenced, so the fallback changes no successor,
    /// predecessor, or finger (it may still invalidate finger-convergence
    /// evidence, which this model does not drive).
    pub(super) fn observe(mut self, event: LifecycleEvent, overlay: &Overlay) -> NodeStep {
        self.events.remove(&event);
        let event = match overlay.mutation() {
            ShellMutation::CallbackIgnoresGeneration => self
                .lifecycles
                .state(event.attempt().peer())
                .map_or(event, |lifecycle| event.readdressed(lifecycle.attempt())),
            ShellMutation::Faithful
            | ShellMutation::ReplacementPreserves
            | ShellMutation::RetireWithoutRemove => event,
        };
        match event {
            LifecycleEvent::ChannelOpened(attempt) => self.admit(attempt, overlay),
            LifecycleEvent::SendTerminal(attempt) => {
                if self.lifecycles.mark_send_terminal(attempt) {
                    self.events
                        .insert(LifecycleEvent::RetireUnavailable(attempt));
                }
            }
            LifecycleEvent::RetireUnavailable(attempt) => {
                self.retire(attempt, DhtPeerRemoval::Unavailable, overlay);
            }
            LifecycleEvent::Closed(attempt) => {
                if !self.lifecycles.remove_unadmitted(attempt) {
                    self.retire(attempt, DhtPeerRemoval::Ordinary, overlay);
                }
            }
        }
        NodeStep {
            node: self,
            effects: Vec::new(),
        }
    }

    /// `commit_connection_admission`: `Pending(g) → Admitting(g)`, then the
    /// topology `Admit` and `Admitting(g) → Active(g)` as one step.
    ///
    /// Production also requires transport readiness at commit; the model
    /// admits a generation whose link died before the channel-open event was
    /// delivered and retires it on the queued close, a superset schedule.
    fn admit(&mut self, attempt: PendingConnectionAttempt, overlay: &Overlay) {
        self.lifecycles.begin_admission(attempt);
        let Some(admitting) = self.lifecycles.admitting_connection(attempt) else {
            return;
        };
        let next = step(
            &self.topology,
            TopologyEvent::Admit {
                peer: attempt.peer(),
                deferred_proof: None,
                now_ms: ADMITTED_AT_MS,
            },
            overlay.successor_capacity(),
        );
        admitting.activate();
        self.topology = next.state;
    }

    /// `retire_active_if(g, …)` composed with the topology `Remove`: the
    /// registry decides, under one borrow, whether `g` still owns the active
    /// slot, and only then does the topology change.
    ///
    /// Post (head-replacement law): an `Unavailable` retirement of the head
    /// leaves `succ' = Successors(Sendable ∖ {removed}, n, K)`, the sendable
    /// admitted successors; any other result is recorded in
    /// `unreplaced_head`. Under [`ShellMutation::RetireWithoutRemove`] the
    /// registry retires the generation but the topology keeps every
    /// reference to its peer: the composition the lifecycle boundary exists
    /// to make atomic, taken apart.
    fn retire(
        &mut self,
        attempt: PendingConnectionAttempt,
        removal: DhtPeerRemoval,
        overlay: &Overlay,
    ) {
        let topology = &self.topology;
        let removed = attempt.peer();
        let retired = self.lifecycles.retire_active_if(attempt, |active| {
            let successor = match removal {
                DhtPeerRemoval::Ordinary => SuccessorRemoval::Preserve,
                DhtPeerRemoval::Unavailable => replacement_evidence(removed, active, overlay),
            };
            let expected_after_head_replacement = (removal == DhtPeerRemoval::Unavailable
                && successor_head(topology) == Some(removed))
            .then(|| sendable_successors(topology.local, removed, active, overlay));
            let event = TopologyEvent::Remove {
                peer: removed,
                successor,
            };
            let next = match overlay.mutation() {
                ShellMutation::RetireWithoutRemove => topology.clone(),
                ShellMutation::Faithful
                | ShellMutation::CallbackIgnoresGeneration
                | ShellMutation::ReplacementPreserves => {
                    step(topology, event, overlay.successor_capacity()).state
                }
            };
            Ok(Some((next, expected_after_head_replacement)))
        });
        if let Ok(RetirementOutcome::Retired(((topology, expected), _announcement))) = retired {
            if expected.is_some_and(|expected| expected != topology.successors) {
                self.unreplaced_head = Some(UnreplacedHead { removed });
            }
            self.topology = topology;
        }
    }

    /// One maintenance period: `Stabilizer::correct_stabilize` against the
    /// head.
    ///
    /// The periodic `notify_predecessor` broadcast (to every successor, every
    /// period) is not modeled: toward the head it re-sends the message the
    /// committed report already emits (`TopologyAction::Notify`), and the
    /// model's rounds repeat; toward the tail it is omitted, which drops
    /// protocol steps and so makes the liveness result conservative
    /// (`rectify_predecessor` is monotone, so the extra notifications could
    /// not unsettle a predecessor).
    pub(super) fn stabilize(mut self, request_id: uuid::Uuid, overlay: &Overlay) -> NodeStep {
        let actions = self.advance(TopologyEvent::BeginStabilize { request_id }, overlay);
        let effects = self.interpret(actions, overlay);
        NodeStep {
            node: self,
            effects,
        }
    }

    /// Deliver one inbound message that arrived on generation `via`.
    ///
    /// A message whose generation is not the admitted one is consumed
    /// without effect: production parks such a frame in the pre-admission
    /// hold and discards the hold when the retired generation's close is
    /// torn down. The caller keeps a message whose generation is still in
    /// the hold. A `Notify` additionally requires the sender to be sendable
    /// (`notify_admitted_predecessor`).
    pub(super) fn receive(
        mut self,
        via: PendingConnectionAttempt,
        message: ProtocolMessage,
        overlay: &Overlay,
    ) -> NodeStep {
        let mut effects = Vec::new();
        if self.admits(via) {
            match message {
                ProtocolMessage::TopologyQuery { request_id } => {
                    let report = ProtocolMessage::TopologyReport {
                        request_id,
                        successors: self.topology.successors.clone(),
                        predecessor: self.topology.predecessor,
                    };
                    effects.extend(self.message_to(via.peer(), report));
                }
                ProtocolMessage::NotifyPredecessor => {
                    if self.sendable(via) {
                        self.advance(
                            TopologyEvent::Notify {
                                predecessor: via.peer(),
                            },
                            overlay,
                        );
                    }
                }
                ProtocolMessage::TopologyReport {
                    request_id,
                    successors,
                    predecessor,
                } => {
                    let reported = TopoInfo {
                        successors,
                        predecessor,
                    };
                    effects = self.stabilize_reported_by(via.peer(), request_id, reported, overlay);
                }
            }
        }
        NodeStep {
            node: self,
            effects,
        }
    }

    /// `handle_stabilization_report`: claim the token, spend the production
    /// connection plan (one offer per unknown candidate), then
    /// `stabilize_routable_topology` and the claim drop, as one step.
    ///
    /// ```text
    /// [can claim (reporter, token)?] ── no ──▶ inert (stale or duplicate)
    ///            │ yes: ClaimStabilize
    ///            ▼
    /// [plan.advance ⇒ Connect(c)]* ──▶ offer to every unknown candidate
    ///            ▼
    /// [reporter sendable ∧ some reported peer sendable?] ── no ──▶ release
    ///            │ yes: Stabilize(confirmed report)
    ///            ▼
    /// CancelStabilize (a no-op once Stabilize retired the token)
    /// ```
    ///
    /// Production's `connect_dht_peer` awaits only the offer send, so the
    /// commit follows the offers without waiting for a handshake; a candidate
    /// admitted later is confirmed by the next round. The per-candidate
    /// revalidation of the plan under churn is the subject of the
    /// finger-retry model (Stage 5).
    fn stabilize_reported_by(
        &mut self,
        reporter: Did,
        request_id: uuid::Uuid,
        reported: TopoInfo,
        overlay: &Overlay,
    ) -> Vec<Effect> {
        if !self
            .topology
            .can_claim_stabilization_report(reporter, request_id)
        {
            return Vec::new();
        }
        self.advance(
            TopologyEvent::ClaimStabilize {
                reporter,
                request_id,
            },
            overlay,
        );
        let local = self.topology.local;
        let mut plan = ConnectionPlan::new(
            reporter,
            request_id,
            reported
                .predecessor
                .into_iter()
                .chain(reported.successors.iter().copied()),
            local,
            stabilization_connection_budget(overlay.successor_capacity()),
        );
        let mut effects = Vec::new();
        while let ConnectionStep::Connect(candidate) = plan.advance(|reporter, request_id| {
            self.topology
                .is_processing_stabilization_report(reporter, request_id)
        }) {
            effects.extend(self.offer_to(candidate));
        }
        let confirmed = reported
            .confirmed_by(|peer| peer == local || self.lifecycles.sendable_attempt(peer).is_some());
        if self.lifecycles.sendable_attempt(reporter).is_some() && confirmed.has_confirmed_peer() {
            let actions = self.advance(
                TopologyEvent::Stabilize {
                    reporter,
                    request_id,
                    successors: confirmed.successors,
                    predecessor: confirmed.predecessor,
                },
                overlay,
            );
            effects.extend(self.interpret(actions, overlay));
        }
        self.advance(TopologyEvent::CancelStabilize { request_id }, overlay);
        effects
    }
}

/// `Sendable ∖ {removed}`: the peers an unavailable head may be replaced
/// with, as production's `live_successor_replacements_from_active` gathers
/// them before normalizing, up to transport readiness.
fn sendable_candidates(removed: Did, active: &ActiveConnectionSet) -> Vec<Did> {
    active
        .iter()
        .map(PendingConnectionAttempt::peer)
        .filter(|peer| *peer != removed)
        .collect()
}

/// `Successors(Sendable ∖ {removed}, n, K)`: the normalized image of
/// [`sendable_candidates`], the post-state of the head-replacement law.
fn sendable_successors(
    local: Did,
    removed: Did,
    active: &ActiveConnectionSet,
    overlay: &Overlay,
) -> Vec<Did> {
    successors(
        &sendable_candidates(removed, active),
        local,
        overlay.successor_capacity(),
    )
}

/// The successor evidence an `Unavailable` retirement hands to `Remove`.
///
/// Production sorts the sendable admitted peers clockwise, drops `removed`,
/// and truncates to capacity, and only when `removed` is the head. `step`'s
/// `Remove` applies that same normalization to whatever it is given and
/// ignores the list for a non-head, so handing it the unnormalized
/// [`sendable_candidates`] is observationally equal (witnessed by
/// `test_replacement_normalization_is_absorbed_by_the_production_remove`).
///
/// Under [`ShellMutation::ReplacementPreserves`] the head is not replaced at
/// all: the `Ordinary` flavour where production selects `Unavailable`.
fn replacement_evidence(
    removed: Did,
    active: &ActiveConnectionSet,
    overlay: &Overlay,
) -> SuccessorRemoval {
    match overlay.mutation() {
        ShellMutation::ReplacementPreserves => SuccessorRemoval::Preserve,
        ShellMutation::Faithful
        | ShellMutation::CallbackIgnoresGeneration
        | ShellMutation::RetireWithoutRemove => {
            SuccessorRemoval::ReplaceWith(sendable_candidates(removed, active))
        }
    }
}
