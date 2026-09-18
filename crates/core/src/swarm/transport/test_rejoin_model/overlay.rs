//! The environment of the composed model and its next-state relation.
//!
//! The environment owns exactly what no peer owns: which peers are up, which
//! physical links exist, which envelopes are in flight, and how much
//! adversarial budget remains. It never decides a protocol question; it
//! dispatches the effects that [`NodeState`] transitions request and raises
//! the lifecycle events a physical change makes observable.
//!
//! ```text
//!            environment action                     protocol action
//!   Depart │ Rejoin │ Cut │ Lose │ Duplicate      Dial │ Deliver │ Observe │ Stabilize
//!                  │                                         │
//!                  ▼                                         ▼
//!     [links, network, events raised]           [NodeState transition ⇒ effects]
//!                  │                                         │
//!                  └──────────────▶ OverlayState ◀───────────┘
//!                                        │
//!             [retired event changed (topology, lifecycles)?] ⇒ history variable
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use num_bigint::BigUint;

use super::node::Effect;
use super::node::LifecycleEvent;
use super::node::NodeState;
use super::node::NodeStep;
use super::node::OwnOfferState;
use super::node::ProtocolMessage;
use crate::dht::topology::successor_head;
use crate::dht::topology::RING_BITS;
use crate::dht::Did;
use crate::swarm::transport::pending::PendingConnectionAttempt;

/// The one stabilization token the model ever issues.
///
/// Production draws a random UUID per round so a delayed report of an earlier
/// round cannot claim a later one. Under the one-round-at-a-time bound
/// ([`Overlay::may_stabilize`]) no query, report, or notification of an
/// earlier round is in flight when a round begins, so token equality across
/// rounds is unobservable and one constant is a faithful representative. The
/// cross-round stale-token paths (`can_claim_stabilization_report`) are the
/// subject of the finger-retry model (Stage 5).
const STABILIZATION_REQUEST_ID: uuid::Uuid = uuid::Uuid::from_u128(1);

/// A deliberate defect of the shell interpretation.
///
/// The faithful model must satisfy every law; each mutation removes one guard
/// so a test can show the corresponding law is falsifiable, with a minimal
/// counterexample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShellMutation {
    /// The production composition.
    Faithful,
    /// Lifecycle events are resolved by peer instead of by generation: the
    /// exact-generation guard is removed.
    CallbackIgnoresGeneration,
    /// An unavailable head is removed without replacement: the
    /// successor-replacement invariant is removed.
    ReplacementPreserves,
    /// The topology admits a peer before its generation is activated: the
    /// admission order of the lifecycle boundary is removed.
    AdmitBeforeActivation,
}

/// An adversarial resource the environment spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resource {
    /// A peer goes down.
    Departure,
    /// A departed peer starts again.
    Rejoin,
    /// A link dies between two live peers.
    Cut,
    /// The network drops a message.
    Loss,
    /// The network delivers a message twice.
    Duplication,
    /// An offer reaches an addressee that refuses it.
    Refusal,
}

/// Remaining adversarial budget per resource; the only unbounded behaviour
/// of the environment made finite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Budget {
    /// Peers that may still go down.
    pub(super) departures: u8,
    /// Departed peers that may still start again.
    pub(super) rejoins: u8,
    /// Links that may still die between two live peers.
    pub(super) cuts: u8,
    /// Messages the network may still drop.
    pub(super) loss: u8,
    /// Messages the network may still deliver twice.
    pub(super) duplication: u8,
    /// Offers that may still reach an addressee that refuses them.
    ///
    /// A refused offer burns one generation at the offerer, so unbounded
    /// refusal would make the generation counter, and the carrier, infinite.
    /// Once the budget is spent a refusable offer waits in the network until
    /// its addressee would answer it, which is what the retry schedule of a
    /// real offerer converges to.
    pub(super) refusal: u8,
}

impl Budget {
    /// The remaining amount of `resource`.
    const fn remaining(self, resource: Resource) -> u8 {
        match resource {
            Resource::Departure => self.departures,
            Resource::Rejoin => self.rejoins,
            Resource::Cut => self.cuts,
            Resource::Loss => self.loss,
            Resource::Duplication => self.duplication,
            Resource::Refusal => self.refusal,
        }
    }

    /// `Permits(r)`: at least one unit of `resource` remains.
    pub(super) const fn permits(self, resource: Resource) -> bool {
        self.remaining(resource) > 0
    }

    /// The budget after spending one unit of `resource`, or `None` when none
    /// remains: the environment step is then disabled.
    fn spent(self, resource: Resource) -> Option<Self> {
        let left = self.remaining(resource).checked_sub(1)?;
        Some(match resource {
            Resource::Departure => Self {
                departures: left,
                ..self
            },
            Resource::Rejoin => Self {
                rejoins: left,
                ..self
            },
            Resource::Cut => Self { cuts: left, ..self },
            Resource::Loss => Self { loss: left, ..self },
            Resource::Duplication => Self {
                duplication: left,
                ..self
            },
            Resource::Refusal => Self {
                refusal: left,
                ..self
            },
        })
    }
}

/// A physical connection: the two generations, one per endpoint, that were
/// paired by one accepted offer.
///
/// An end `g` is held by the peer the *other* end points at, so the pair
/// determines both holders and no holder field can disagree with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Link {
    /// The lesser end in the derived order (canonical form of the pair).
    lesser: PendingConnectionAttempt,
    /// The greater end in the derived order.
    greater: PendingConnectionAttempt,
}

impl Link {
    /// Pair two ends; the argument order is not observable.
    fn pairing(one: PendingConnectionAttempt, other: PendingConnectionAttempt) -> Self {
        Self {
            lesser: one.min(other),
            greater: one.max(other),
        }
    }

    /// Both `(holder, end)` incidences of the link.
    fn incidences(self) -> [(Did, PendingConnectionAttempt); 2] {
        [
            (self.greater.peer(), self.lesser),
            (self.lesser.peer(), self.greater),
        ]
    }

    /// The end at the far side of the end `near` held by `holder`.
    fn far_end(
        self,
        holder: Did,
        near: PendingConnectionAttempt,
    ) -> Option<PendingConnectionAttempt> {
        let [one, other] = self.incidences();
        if one == (holder, near) {
            Some(other.1)
        } else if other == (holder, near) {
            Some(one.1)
        } else {
            None
        }
    }

    /// Whether `peer` holds one of the ends.
    fn touches(self, peer: Did) -> bool {
        self.incidences().iter().any(|(holder, _)| *holder == peer)
    }
}

/// A message in flight. Delivery order is unconstrained: the network is a set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Envelope {
    /// Signaled offer from `offerer` for its generation `offered`.
    Offer {
        /// Peer that reserved `offered`.
        offerer: Did,
        /// Offerer's generation; `offered.peer()` is the addressee.
        offered: PendingConnectionAttempt,
    },
    /// A protocol message on a link, tagged with the receiver-local
    /// generation `via` it arrives on; `via.peer()` is the sender.
    Message {
        /// Addressee.
        to: Did,
        /// Receiver's generation of the carrying link.
        via: PendingConnectionAttempt,
        /// Payload.
        message: ProtocolMessage,
    },
}

impl Envelope {
    /// The peer that would process this envelope.
    fn addressee(&self) -> Did {
        match self {
            Self::Offer { offered, .. } => offered.peer(),
            Self::Message { to, .. } => *to,
        }
    }

    /// Whether this envelope belongs to a stabilization round: its query,
    /// its report, or the notification its commit emits.
    const fn carries_stabilization_round(&self) -> bool {
        matches!(self, Self::Message { .. })
    }
}

/// A retired generation's event that changed protected state: the witness of
/// a violated exact-generation guard, recorded as a history variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct StaleEffect {
    /// Peer whose protected state changed.
    pub(super) peer: Did,
    /// The retired generation the event was bound to.
    pub(super) retired: PendingConnectionAttempt,
}

/// The carrier of the model.
///
/// Peers are shared between a state and its successors (`Arc`): a transition
/// copies only the peer it changes, so a breadth-first frontier of a hundred
/// thousand states holds each unchanged peer once. Equality and hashing see
/// through the sharing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct OverlayState {
    /// Peers that are up.
    pub(super) nodes: BTreeMap<Did, Arc<NodeState>>,
    /// Physical links that are alive.
    pub(super) links: BTreeSet<Link>,
    /// Envelopes in flight.
    pub(super) network: BTreeSet<Envelope>,
    /// Remaining adversarial budget.
    pub(super) remaining: Budget,
    /// History variable of the retired-generation law.
    pub(super) stale_effect: Option<StaleEffect>,
}

/// One transition label.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum OverlayAction {
    /// A peer goes down; its links die and its in-flight input is lost.
    Depart(Did),
    /// A departed peer starts again with empty state.
    Rejoin(Did),
    /// A link dies while both endpoints stay up.
    Cut(Link),
    /// The network drops one message.
    Lose(Envelope),
    /// The network delivers one message and keeps a copy.
    Duplicate(Envelope),
    /// An isolated peer dials a bootstrap contact.
    Dial {
        /// The isolated peer.
        from: Did,
        /// A peer that is up.
        to: Did,
    },
    /// The network delivers one envelope and consumes it.
    Deliver(Envelope),
    /// A peer's shell delivers one local event.
    Observe {
        /// Peer whose event fires.
        peer: Did,
        /// The event.
        event: LifecycleEvent,
    },
    /// One maintenance period of a peer.
    Stabilize(Did),
}

impl OverlayAction {
    /// Whether the adversarial environment, rather than the protocol, takes
    /// this step. The quiescent suffix is the behaviours with no such step.
    pub(super) const fn is_environmental(&self) -> bool {
        matches!(
            self,
            Self::Depart(_) | Self::Rejoin(_) | Self::Cut(_) | Self::Lose(_) | Self::Duplicate(_)
        )
    }

    /// Whether a production timer drives this step. Such a step is
    /// continuously enabled in production, and only the search's
    /// one-round-at-a-time bound disables it, so its fairness is strong.
    pub(super) const fn is_periodic(&self) -> bool {
        matches!(self, Self::Stabilize(_))
    }
}

/// The model: a ring of peer identities and the bounds of the search.
#[derive(Debug)]
pub(super) struct Overlay {
    /// Every modeled identity, evenly spaced on `Z/2^160` from the origin.
    ring: Vec<Did>,
    /// Successor-list capacity given to `topology::step`.
    successor_capacity: usize,
    /// Width of each modeled finger table.
    finger_slots: usize,
    /// Initial adversarial budget.
    budget: Budget,
    /// Shell defect under test, `Faithful` for the verified model.
    mutation: ShellMutation,
}

impl Overlay {
    /// A ring of `peers` identities at `origin + i · 2^160 / peers`.
    ///
    /// Pre: `peers ≥ 1`; the divisor is clamped so the constructor is total.
    ///
    /// Even spacing makes every finger threshold `2^i` below the modeled
    /// width smaller than any inter-peer distance when the table is narrow,
    /// so each narrow finger's fixpoint is the successor head: the part of
    /// the finger table stabilization itself proves. Remote finger ranges
    /// belong to the finger-retry model (Stage 5). The protocol is
    /// equivariant under rotation, so `origin` only matters when the ring has
    /// to contain a given production identity.
    pub(super) fn new(
        origin: Did,
        peers: u32,
        successor_capacity: usize,
        finger_slots: usize,
        budget: Budget,
        mutation: ShellMutation,
    ) -> Self {
        let ring = (0..peers)
            .map(|position| {
                origin + Did::from((BigUint::from(1u8) << RING_BITS) * position / peers.max(1))
            })
            .collect();
        Self {
            ring,
            successor_capacity,
            finger_slots,
            budget,
            mutation,
        }
    }

    /// Every modeled identity.
    pub(super) fn ring(&self) -> &[Did] {
        self.ring.as_slice()
    }

    /// Successor-list capacity.
    pub(super) const fn successor_capacity(&self) -> usize {
        self.successor_capacity
    }

    /// Finger-table width.
    pub(super) const fn finger_slots(&self) -> usize {
        self.finger_slots
    }

    /// Shell defect under test.
    pub(super) const fn mutation(&self) -> ShellMutation {
        self.mutation
    }

    /// `Init`: every peer up, a full mesh of admitted links, and one settled
    /// maintenance period per peer: the Chord fixpoint of the whole ring,
    /// built by the same transitions the search uses.
    pub(super) fn converged_mesh(&self) -> OverlayState {
        let mut state = OverlayState {
            nodes: self
                .ring
                .iter()
                .map(|peer| (*peer, Arc::new(NodeState::started(*peer, self))))
                .collect(),
            links: BTreeSet::new(),
            network: BTreeSet::new(),
            remaining: self.budget,
            stale_effect: None,
        };
        let pairs = self
            .ring
            .iter()
            .flat_map(|from| self.ring.iter().map(move |to| (*from, *to)))
            .filter(|(from, to)| from < to);
        for (from, to) in pairs {
            state = self.delivered_closure(self.transition(&state, from, |node| node.dial(to)));
        }
        for peer in self.ring.iter().copied() {
            let round = self.next_state(&state, &OverlayAction::Stabilize(peer));
            state = self.delivered_closure(round.unwrap_or(state));
        }
        state
    }

    /// Deliver every envelope and event until none is left, in action
    /// order: a deterministic churn-free schedule in which no peer dials or
    /// starts a period, used only to build `Init`.
    fn delivered_closure(&self, mut state: OverlayState) -> OverlayState {
        loop {
            let Some(next) = self
                .actions(&state)
                .iter()
                .filter(|action| {
                    matches!(
                        action,
                        OverlayAction::Deliver(_) | OverlayAction::Observe { .. }
                    )
                })
                .find_map(|action| self.next_state(&state, action))
            else {
                return state;
            };
            state = next;
        }
    }

    /// Apply a node transition at `peer` and dispatch its effects.
    ///
    /// This is the one place a node's state is replaced, so it is also the
    /// one place the retired-generation law is observed (see
    /// [`Self::transition_bound_to`]). The peer is shared with the previous
    /// state, so the transition works on a copy of it.
    fn transition(
        &self,
        state: &OverlayState,
        peer: Did,
        apply: impl FnOnce(NodeState) -> NodeStep,
    ) -> OverlayState {
        let mut next = state.clone();
        let Some(node) = next.nodes.remove(&peer) else {
            return next;
        };
        let NodeStep { node, effects } = apply(node.as_ref().clone());
        next.nodes.insert(peer, Arc::new(node));
        for effect in effects {
            next.dispatch(peer, effect);
        }
        next
    }

    /// [`Self::transition`] for an input bound to generation `bound`.
    ///
    /// Law (retired generations are inert): `¬Owns(bound) ⇒ observable' =
    /// observable`. A violation is recorded in the history variable rather
    /// than asserted, so the search reports it with a minimal trace.
    fn transition_bound_to(
        &self,
        state: &OverlayState,
        peer: Did,
        bound: PendingConnectionAttempt,
        apply: impl FnOnce(NodeState) -> NodeStep,
    ) -> OverlayState {
        let mut next = self.transition(state, peer, apply);
        let changed_by_retired = state.nodes.get(&peer).is_some_and(|before| {
            !before.owns(bound)
                && next.nodes.get(&peer).is_some_and(|after| {
                    after.retirement_observable() != before.retirement_observable()
                })
        });
        if changed_by_retired {
            next.stale_effect = Some(StaleEffect {
                peer,
                retired: bound,
            });
        }
        next
    }

    /// Whether `peer` may start a maintenance period: it has a head to query
    /// and no stabilization round is in progress anywhere in the overlay.
    ///
    /// One round at a time is a bound of the search, not of the protocol:
    /// rounds of different peers touch disjoint topology states and read each
    /// other only through a report, so serializing them loses the races in
    /// which one peer queries a head whose predecessor another peer's
    /// notification is about to change — races that reorder convergence, not
    /// lifecycle evidence. Every lifecycle event still interleaves with the
    /// round. The bound also makes the token space trivial
    /// ([`STABILIZATION_REQUEST_ID`]); a lost query leaves nothing in flight,
    /// so the next period supersedes it as production does.
    fn may_stabilize(state: &OverlayState, node: &NodeState) -> bool {
        successor_head(&node.topology).is_some()
            && !state
                .network
                .iter()
                .any(Envelope::carries_stabilization_round)
    }

    /// Deliver `envelope` to its addressee; `None` when the environment has
    /// no budget for the refusal it would cause.
    fn delivered(&self, state: &OverlayState, envelope: &Envelope) -> Option<OverlayState> {
        match envelope {
            Envelope::Offer { offerer, offered } => self.offer_delivered(state, *offerer, *offered),
            Envelope::Message { to, via, message } => {
                Some(self.transition_bound_to(state, *to, *via, |node| {
                    node.receive(*via, message.clone(), self)
                }))
            }
        }
    }

    /// The addressee answers or refuses an offer.
    ///
    /// An offer *stands* while its offerer is up and still owns the offered
    /// generation; one that does not is void, and signaling drops it without
    /// consulting the addressee. A refusal has no reply in production; the
    /// offerer's generation later expires. The model raises that expiry as
    /// the offered generation's `Closed`, deliverable at any later time.
    ///
    /// ```text
    /// [offer stands?] ── no ──▶ dropped
    ///       │ yes
    ///       ▼
    /// [answer_offer] ── answered ──▶ link paired, channel opens at both ends
    ///       │ refused
    ///       ▼
    /// [refusal budget] ── spent ──▶ not enabled (the offer waits)
    ///       │ available
    ///       ▼
    /// offered generation closes at the offerer
    /// ```
    fn offer_delivered(
        &self,
        state: &OverlayState,
        offerer: Did,
        offered: PendingConnectionAttempt,
    ) -> Option<OverlayState> {
        let stands = state
            .nodes
            .get(&offerer)
            .is_some_and(|node| node.owns(offered));
        if !stands {
            return Some(state.clone());
        }
        let addressee = offered.peer();
        let own_offer = state
            .nodes
            .get(&addressee)
            .and_then(|node| node.lifecycles.pending_attempt(offerer))
            .filter(|pending| state.far_end_of(addressee, *pending).is_some())
            .map_or(OwnOfferState::Unanswered, |_| OwnOfferState::Answered);
        let mut answered = None;
        let mut next = self.transition(state, addressee, |node| {
            let (node, answer) = node.answer_offer(offerer, own_offer, self);
            answered = answer;
            NodeStep {
                node,
                effects: Vec::new(),
            }
        });
        match answered {
            Some(answer) => {
                next.links.insert(Link::pairing(offered, answer));
                next.queue_event(offerer, LifecycleEvent::ChannelOpened(offered));
                next.queue_event(addressee, LifecycleEvent::ChannelOpened(answer));
            }
            None => {
                next.remaining = next.remaining.spent(Resource::Refusal)?;
                next.queue_event(offerer, LifecycleEvent::Closed(offered));
            }
        }
        Some(next)
    }
}

impl OverlayState {
    /// The live member set `M` the fixpoint is judged against.
    pub(super) fn members(&self) -> Vec<Did> {
        self.nodes.keys().copied().collect()
    }

    /// The far end of the live link whose end `near` is held by `holder`, if
    /// that link is alive.
    pub(super) fn far_end_of(
        &self,
        holder: Did,
        near: PendingConnectionAttempt,
    ) -> Option<PendingConnectionAttempt> {
        self.links
            .iter()
            .find_map(|link| link.far_end(holder, near))
    }

    /// Whether a recorded violation ended this behaviour: the trace stops at
    /// its witness.
    pub(super) fn has_recorded_violation(&self) -> bool {
        self.stale_effect.is_some()
            || self
                .nodes
                .values()
                .any(|node| node.unreplaced_head.is_some())
    }

    /// Queue `event` at `peer`, if it is up.
    fn queue_event(&mut self, peer: Did, event: LifecycleEvent) {
        if let Some(node) = self.nodes.get_mut(&peer) {
            Arc::make_mut(node).events.insert(event);
        }
    }

    /// Dispatch one effect of `sender`.
    ///
    /// A message sent under a generation whose link is already dead is lost:
    /// the sender has not yet observed the failure that the raised events
    /// will report.
    fn dispatch(&mut self, sender: Did, effect: Effect) {
        match effect {
            Effect::Offer { offered } => {
                self.network.insert(Envelope::Offer {
                    offerer: sender,
                    offered,
                });
            }
            Effect::Message { under, message } => {
                if let Some(via) = self.far_end_of(sender, under) {
                    self.network.insert(Envelope::Message {
                        to: under.peer(),
                        via,
                        message,
                    });
                }
            }
        }
    }

    /// A link dies: both holders will observe a send failure and a close of
    /// exactly the generation that carried it.
    ///
    /// Production marks send-terminal only when a send fails, and its sweep
    /// may retire a dead link's generation while it is still sendable; the
    /// two orders reach the same state (retirement clears the send-terminal
    /// mark), so raising both events is the more general schedule.
    fn cut(&mut self, link: Link) {
        self.links.remove(&link);
        for (holder, end) in link.incidences() {
            self.queue_event(holder, LifecycleEvent::SendTerminal(end));
            self.queue_event(holder, LifecycleEvent::Closed(end));
        }
    }

    /// `peer` goes down: its links die, every handshake toward it closes, and
    /// everything addressed to or signaled by it is lost with the process.
    fn depart(&mut self, peer: Did) {
        self.nodes.remove(&peer);
        let severed = self
            .links
            .iter()
            .copied()
            .filter(|link| link.touches(peer))
            .collect::<Vec<_>>();
        for link in severed {
            self.cut(link);
        }
        let handshaking = self
            .nodes
            .iter()
            .filter_map(|(holder, node)| {
                node.lifecycles
                    .unadmitted_attempt(peer)
                    .map(|unadmitted| (*holder, unadmitted))
            })
            .collect::<Vec<_>>();
        for (holder, unadmitted) in handshaking {
            self.queue_event(holder, LifecycleEvent::Closed(unadmitted));
        }
        self.network.retain(|envelope| {
            envelope.addressee() != peer
                && !matches!(envelope, Envelope::Offer { offerer, .. } if *offerer == peer)
        });
    }
}

/// The next-state relation, as the enabled actions of a state and their
/// results. The search enumerates `actions` in this order, so a trace is
/// replayable from action indices.
impl Overlay {
    /// The actions enabled at `state`, in canonical order.
    ///
    /// A recorded violation is terminal: the trace ends at its witness. An
    /// offer whose refusal is unbudgeted is listed here and disabled by
    /// [`Self::next_state`], which alone can tell whether it is refused.
    pub(super) fn actions(&self, state: &OverlayState) -> Vec<OverlayAction> {
        let mut actions = Vec::new();
        if state.has_recorded_violation() {
            return actions;
        }
        if state.remaining.permits(Resource::Departure) {
            actions.extend(state.nodes.keys().copied().map(OverlayAction::Depart));
        }
        if state.remaining.permits(Resource::Rejoin) {
            actions.extend(
                self.ring
                    .iter()
                    .copied()
                    .filter(|peer| !state.nodes.contains_key(peer))
                    .map(OverlayAction::Rejoin),
            );
        }
        if state.remaining.permits(Resource::Cut) {
            actions.extend(state.links.iter().copied().map(OverlayAction::Cut));
        }
        for envelope in state.network.iter() {
            let Some(addressee) = state.nodes.get(&envelope.addressee()) else {
                continue;
            };
            match envelope {
                Envelope::Offer { .. } => actions.push(OverlayAction::Deliver(envelope.clone())),
                Envelope::Message { via, .. } if addressee.holds_before_admission(*via) => {}
                Envelope::Message { .. } => {
                    actions.push(OverlayAction::Deliver(envelope.clone()));
                    if state.remaining.permits(Resource::Loss) {
                        actions.push(OverlayAction::Lose(envelope.clone()));
                    }
                    if state.remaining.permits(Resource::Duplication) {
                        actions.push(OverlayAction::Duplicate(envelope.clone()));
                    }
                }
            }
        }
        for (peer, node) in state.nodes.iter() {
            actions.extend(node.events.iter().map(|event| OverlayAction::Observe {
                peer: *peer,
                event: *event,
            }));
            if node.is_isolated(self) {
                actions.extend(state.nodes.keys().filter(|contact| *contact != peer).map(
                    |contact| OverlayAction::Dial {
                        from: *peer,
                        to: *contact,
                    },
                ));
            }
            if Self::may_stabilize(state, node) {
                actions.push(OverlayAction::Stabilize(*peer));
            }
        }
        actions
    }

    /// The state `action` leads to from `state`, or `None` when the action
    /// is disabled after all (an unbudgeted refusal) or changes nothing.
    pub(super) fn next_state(
        &self,
        state: &OverlayState,
        action: &OverlayAction,
    ) -> Option<OverlayState> {
        let next = match action {
            OverlayAction::Depart(peer) => {
                let mut next = state.clone();
                next.remaining = next.remaining.spent(Resource::Departure)?;
                next.depart(*peer);
                next
            }
            OverlayAction::Rejoin(peer) => {
                let mut next = state.clone();
                next.remaining = next.remaining.spent(Resource::Rejoin)?;
                next.nodes
                    .insert(*peer, Arc::new(NodeState::started(*peer, self)));
                next
            }
            OverlayAction::Cut(link) => {
                let mut next = state.clone();
                next.remaining = next.remaining.spent(Resource::Cut)?;
                next.cut(*link);
                next
            }
            OverlayAction::Lose(envelope) => {
                let mut next = state.clone();
                next.remaining = next.remaining.spent(Resource::Loss)?;
                next.network.remove(envelope);
                next
            }
            OverlayAction::Duplicate(envelope) => {
                let mut next = self.delivered(state, envelope)?;
                next.remaining = next.remaining.spent(Resource::Duplication)?;
                next
            }
            OverlayAction::Deliver(envelope) => {
                let mut next = self.delivered(state, envelope)?;
                next.network.remove(envelope);
                next
            }
            OverlayAction::Dial { from, to } => {
                self.transition(state, *from, |node| node.dial(*to))
            }
            OverlayAction::Observe { peer, event } => {
                self.transition_bound_to(state, *peer, event.attempt(), |node| {
                    node.observe(*event, self)
                })
            }
            OverlayAction::Stabilize(peer) => self.transition(state, *peer, |node| {
                node.stabilize(STABILIZATION_REQUEST_ID, self)
            }),
        };
        (next != *state).then_some(next)
    }
}
