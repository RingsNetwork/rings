//! One peer of the composed model: the production topology state and the
//! production lifecycle registry, advanced by the shell interpretation that
//! joins them.
//!
//! Nothing here re-implements a production state machine. Every topology
//! change is `topology::step`, every lifecycle change is a
//! [`ConnectionLifecycleRegistry`] method, and the bounded candidate budget is
//! the production [`StabilizationConnectionPlan`]. What this module owns is the
//! *composition* that production performs inside `SwarmTransport` under the
//! lifecycle boundary, written as pure functions `NodeState × Input →
//! NodeState × [Effect]`:
//!
//! ```text
//! callback(attempt) ──▶ [registry: does `attempt` own the slot?] ── no ──▶ inert
//!                                   │ yes
//!                                   ▼
//!                 [registry transition] ⨯ [topology::step] (one atomic step)
//!                                   │
//!                                   ▼
//!                        [effects: frames, offers]
//! ```
//!
//! Each function names the production path it interprets, so a reviewer can
//! hold the two side by side.

use std::collections::BTreeSet;

use super::overlay::Overlay;
use super::overlay::ShellMutation;
use crate::dht::topology::step;
use crate::dht::topology::StabilizationConnectionPlan;
use crate::dht::topology::StabilizationConnectionStep;
use crate::dht::topology::SuccessorRemoval;
use crate::dht::topology::TopologyAction;
use crate::dht::topology::TopologyEvent;
use crate::dht::topology::TopologyState;
use crate::dht::Did;
use crate::dht::TopoInfo;
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
/// `Callback(g)` is addressed to generation `g`, not to a peer: the receiving
/// shell must decide whether `g` still owns the peer's slot. That decision is
/// the exact-generation guard under test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Callback {
    /// The data channel of this generation opened (`on_data_channel_open`).
    ChannelOpened(PendingConnectionAttempt),
    /// A send on this generation failed terminally (`mark_send_terminal`).
    SendTerminal(PendingConnectionAttempt),
    /// The unavailable-peer sweep selected this send-terminal generation
    /// (`disconnect_unavailable`).
    RetireUnavailable(PendingConnectionAttempt),
    /// The transport of this generation reached `Failed`/`Closed`
    /// (`leave_dht_attempt`), or its handshake was refused.
    Closed(PendingConnectionAttempt),
}

impl Callback {
    /// The generation this callback was bound to when its connection was made.
    pub(super) const fn attempt(self) -> PendingConnectionAttempt {
        match self {
            Self::ChannelOpened(attempt)
            | Self::SendTerminal(attempt)
            | Self::RetireUnavailable(attempt)
            | Self::Closed(attempt) => attempt,
        }
    }

    /// The same callback re-addressed to `attempt`: the functorial action of a
    /// generation substitution on callbacks, used only by the mutated shell.
    const fn readdressed(self, attempt: PendingConnectionAttempt) -> Self {
        match self {
            Self::ChannelOpened(_) => Self::ChannelOpened(attempt),
            Self::SendTerminal(_) => Self::SendTerminal(attempt),
            Self::RetireUnavailable(_) => Self::RetireUnavailable(attempt),
            Self::Closed(_) => Self::Closed(attempt),
        }
    }
}

/// A stabilization-protocol frame carried by an admitted connection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Frame {
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Send `frame` on the sendable generation `under`.
    Frame {
        /// Local generation the send was admitted under.
        under: PendingConnectionAttempt,
        /// Payload.
        frame: Frame,
    },
    /// Signal an offer for the freshly reserved generation `offered`.
    Offer {
        /// Local generation reserved for the offered peer.
        offered: PendingConnectionAttempt,
    },
}

/// Which production removal a retirement performs on the topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// `DhtPeerRemoval::Ordinary`: `SuccessorRemoval::Preserve`.
    Ordinary,
    /// `DhtPeerRemoval::Unavailable`: `SuccessorRemoval::ReplaceWith` the
    /// admitted, sendable peers.
    Unavailable,
}

/// One live peer: production topology × production lifecycle registry, plus
/// the local events raised but not yet delivered to its shell.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct NodeState {
    /// Production pure topology state.
    pub(super) topology: TopologyState,
    /// Production connection lifecycle registry.
    pub(super) lifecycles: ConnectionLifecycleRegistry,
    /// Local events raised but not yet delivered.
    pub(super) callbacks: BTreeSet<Callback>,
}

/// Result of one node transition: `NodeState × [Effect]`.
pub(super) struct NodeStep {
    /// Next node state.
    pub(super) node: NodeState,
    /// Effects for the environment to route.
    pub(super) effects: Vec<Effect>,
}

impl NodeState {
    /// A freshly started peer: empty topology, empty registry.
    ///
    /// A restart is a new process, so generations restart too; no callback of
    /// the previous process survives to collide with them.
    pub(super) fn started(local: Did, overlay: &Overlay) -> Self {
        let peers = overlay.ring().len();
        Self {
            topology: TopologyState::new(local, Vec::new(), None, vec![
                None;
                overlay.finger_slots()
            ]),
            lifecycles: ConnectionLifecycleRegistry::new(LifecycleBounds::new(peers, peers)),
            callbacks: BTreeSet::new(),
        }
    }

    /// `Owns(g)`: generation `g` is the one recorded for its peer, in any phase.
    ///
    /// `¬Owns(g)` is the definition of a *retired* generation: every event
    /// bound to such a `g` must be inert.
    pub(super) fn owns(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles
            .state(attempt.peer)
            .is_some_and(|lifecycle| lifecycle.attempt() == attempt)
    }

    /// `Holds(g)`: `g` owns its slot but is not yet admitted, so production
    /// parks its inbound frames in the pre-admission hold.
    pub(super) fn holds_before_admission(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles.unadmitted_attempt(attempt.peer) == Some(attempt)
    }

    /// `Admits(g)`: `g` is the admitted generation of its peer, the inbound
    /// gate of `InboundProcessor::pending_connection_gate`.
    fn admits(&self, attempt: PendingConnectionAttempt) -> bool {
        self.lifecycles.active_attempt(attempt.peer) == Some(attempt)
    }

    /// `Isolated`: no lifecycle record exists, so only a bootstrap dial can
    /// reconnect this peer.
    pub(super) fn is_isolated(&self, overlay: &Overlay) -> bool {
        overlay
            .ring()
            .iter()
            .all(|peer| !self.lifecycles.contains(*peer))
    }

    /// The observable of the retired-generation law: everything a stale event
    /// is forbidden to change.
    pub(super) fn protected(&self) -> (&TopologyState, &ConnectionLifecycleRegistry) {
        (&self.topology, &self.lifecycles)
    }

    /// Apply one production topology transition and return its actions.
    fn advance(&mut self, event: TopologyEvent, overlay: &Overlay) -> Vec<TopologyAction> {
        let next = step(&self.topology, event, overlay.successor_capacity());
        self.topology = next.state;
        next.actions
    }

    /// `frame` addressed to `peer`, iff a sendable generation exists
    /// (`sendable_attempt`, the production send gate).
    fn frame_to(&self, peer: Did, frame: Frame) -> Option<Effect> {
        self.lifecycles
            .sendable_attempt(peer)
            .map(|under| Effect::Frame { under, frame })
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
                } => match self.frame_to(successor, Frame::TopologyQuery { request_id }) {
                    Some(effect) => effects.push(effect),
                    None => {
                        self.advance(TopologyEvent::CancelStabilize { request_id }, overlay);
                    }
                },
                TopologyAction::Notify(successor) => {
                    effects.extend(self.frame_to(successor, Frame::NotifyPredecessor));
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
    /// A peer that already owns a record yields no effect (`AlreadyConnected`
    /// is success for `connect_dht_peer`).
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
    /// Returns the answering generation, or `None` when the offer is refused.
    pub(super) fn answer_offer(
        mut self,
        offerer: Did,
        overlay: &Overlay,
    ) -> (Self, Option<PendingConnectionAttempt>) {
        if let Some(admitted) = self.lifecycles.active_attempt(offerer) {
            if self.lifecycles.sendable_attempt(offerer).is_some() {
                return (self, None);
            }
            self.retire(admitted, Removal::Unavailable, overlay);
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

    /// Deliver one local callback.
    ///
    /// Under [`ShellMutation::CallbackIgnoresGeneration`] the callback is first
    /// re-addressed to whatever generation currently owns the peer, which is
    /// the defect the exact-generation guard exists to exclude.
    pub(super) fn observe(mut self, callback: Callback, overlay: &Overlay) -> NodeStep {
        self.callbacks.remove(&callback);
        let callback = match overlay.mutation() {
            ShellMutation::CallbackIgnoresGeneration => self
                .lifecycles
                .state(callback.attempt().peer)
                .map_or(callback, |lifecycle| {
                    callback.readdressed(lifecycle.attempt())
                }),
            ShellMutation::Faithful | ShellMutation::ReplacementIgnoresLifecycle => callback,
        };
        match callback {
            Callback::ChannelOpened(attempt) => self.admit(attempt, overlay),
            Callback::SendTerminal(attempt) => {
                if self.lifecycles.mark_send_terminal(attempt) {
                    self.callbacks.insert(Callback::RetireUnavailable(attempt));
                }
            }
            Callback::RetireUnavailable(attempt) => {
                self.retire(attempt, Removal::Unavailable, overlay);
            }
            Callback::Closed(attempt) => {
                if !self.lifecycles.remove_unadmitted(attempt) {
                    self.retire(attempt, Removal::Ordinary, overlay);
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
    fn admit(&mut self, attempt: PendingConnectionAttempt, overlay: &Overlay) {
        self.lifecycles.begin_admission(attempt);
        let Some(admitting) = self.lifecycles.admitting_connection(attempt) else {
            return;
        };
        let next = step(
            &self.topology,
            TopologyEvent::Admit {
                peer: attempt.peer,
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
    fn retire(&mut self, attempt: PendingConnectionAttempt, removal: Removal, overlay: &Overlay) {
        let topology = &self.topology;
        let retired =
            self.lifecycles.retire_active_if(attempt, |active| {
                let successor =
                    match removal {
                        Removal::Ordinary => SuccessorRemoval::Preserve,
                        Removal::Unavailable => SuccessorRemoval::ReplaceWith(
                            replacement_candidates(topology.local, attempt.peer, active, overlay),
                        ),
                    };
                let event = TopologyEvent::Remove {
                    peer: attempt.peer,
                    successor,
                };
                Ok(Some(
                    step(topology, event, overlay.successor_capacity()).state,
                ))
            });
        if let Ok(RetirementOutcome::Retired((topology, _announcement))) = retired {
            self.topology = topology;
        }
    }

    /// One maintenance period: `Stabilizer::correct_stabilize` against the
    /// head.
    ///
    /// The periodic `notify_predecessor` broadcast is not modeled separately:
    /// toward the head it re-sends the frame the committed report already
    /// emits (`TopologyAction::Notify`), and the model's rounds repeat.
    pub(super) fn stabilize(mut self, request_id: uuid::Uuid, overlay: &Overlay) -> NodeStep {
        let actions = self.advance(TopologyEvent::BeginStabilize { request_id }, overlay);
        let effects = self.interpret(actions, overlay);
        NodeStep {
            node: self,
            effects,
        }
    }

    /// Deliver one inbound frame that arrived on generation `via`.
    ///
    /// A frame whose generation is not the admitted one is consumed without
    /// effect (`InboundGate::Refused`); the caller keeps a frame whose
    /// generation is still in the pre-admission hold.
    pub(super) fn receive(
        mut self,
        via: PendingConnectionAttempt,
        frame: Frame,
        overlay: &Overlay,
    ) -> NodeStep {
        let mut effects = Vec::new();
        if self.admits(via) {
            match frame {
                Frame::TopologyQuery { request_id } => {
                    let report = Frame::TopologyReport {
                        request_id,
                        successors: self.topology.successors.clone(),
                        predecessor: self.topology.predecessor,
                    };
                    effects.extend(self.frame_to(via.peer, report));
                }
                Frame::NotifyPredecessor => {
                    self.advance(
                        TopologyEvent::Notify {
                            predecessor: via.peer,
                        },
                        overlay,
                    );
                }
                Frame::TopologyReport {
                    request_id,
                    successors,
                    predecessor,
                } => {
                    let reported = TopoInfo {
                        successors,
                        predecessor,
                    };
                    effects = self.stabilize_reported_by(via.peer, request_id, reported, overlay);
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
    /// Production awaits each candidate handshake between the claim and the
    /// commit; committing at once is the schedule in which no handshake has
    /// finished, and a candidate admitted later is confirmed by the next
    /// round instead. The per-candidate revalidation of the plan under churn
    /// is the subject of the finger-retry model (Stage 5).
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
        let mut plan = StabilizationConnectionPlan::new(
            reporter,
            request_id,
            reported
                .predecessor
                .into_iter()
                .chain(reported.successors.iter().copied()),
            local,
            overlay.successor_capacity(),
        );
        let mut effects = Vec::new();
        while let StabilizationConnectionStep::Connect { candidate, .. } =
            plan.advance(&self.topology)
        {
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

/// `live_successor_replacements_from_active`: the peers a removed head may be
/// replaced with.
///
/// Production sorts the sendable admitted peers clockwise, drops `removed`,
/// and truncates to capacity, and only when `removed` is the head. `step`'s
/// `Remove` applies that same normalization to whatever it is given and
/// ignores the list for a non-head, so handing it the unnormalized candidate
/// set is observationally equal (witnessed by
/// `test_replacement_normalization_is_absorbed_by_the_production_remove`).
///
/// Under [`ShellMutation::ReplacementIgnoresLifecycle`] the candidates are the
/// whole ring instead: evidence that was never transport-validated.
fn replacement_candidates(
    local: Did,
    removed: Did,
    active: &ActiveConnectionSet,
    overlay: &Overlay,
) -> Vec<Did> {
    match overlay.mutation() {
        ShellMutation::ReplacementIgnoresLifecycle => overlay
            .ring()
            .iter()
            .copied()
            .filter(|peer| *peer != local && *peer != removed)
            .collect(),
        ShellMutation::Faithful | ShellMutation::CallbackIgnoresGeneration => active
            .iter()
            .map(PendingConnectionAttempt::peer)
            .filter(|peer| *peer != removed)
            .collect(),
    }
}
