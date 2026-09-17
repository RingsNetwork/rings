//! The environment of the composed model and its next-state relation.
//!
//! The environment owns exactly what no peer owns: which peers are up, which
//! physical links exist, which envelopes are in flight, and how much
//! adversarial budget remains. It never decides a protocol question; it routes
//! the effects that [`NodeState`] transitions request and raises the callbacks
//! a physical change makes observable.
//!
//! ```text
//!            environment action                     protocol action
//!   Depart │ Rejoin │ Cut │ Lose │ Duplicate   Dial │ Deliver │ Observe │ Stabilize
//!                  │                                         │
//!                  ▼                                         ▼
//!     [links, network, callbacks raised]        [NodeState transition ⇒ effects]
//!                  │                                         │
//!                  └──────────────▶ OverlayState ◀───────────┘
//!                                        │
//!                  [stale event changed (topology, lifecycles)?] ⇒ history variable
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use num_bigint::BigUint;
use stateright::Model;
use stateright::Property;

use super::laws;
use super::node::Callback;
use super::node::Effect;
use super::node::Frame;
use super::node::NodeState;
use super::node::NodeStep;
use crate::dht::topology::successor_head;
use crate::dht::topology::RING_BITS;
use crate::dht::Did;
use crate::swarm::transport::pending::PendingConnectionAttempt;

/// A deliberate defect of the shell interpretation.
///
/// The faithful model must satisfy every law; each mutation removes one guard
/// so a test can show the corresponding law is falsifiable, with a minimal
/// counterexample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShellMutation {
    /// The production composition.
    Faithful,
    /// Callbacks are resolved by peer instead of by generation: the
    /// exact-generation guard is removed.
    CallbackIgnoresGeneration,
    /// Head replacement draws on peers that hold no admitted generation: the
    /// successor-replacement invariant is removed.
    ReplacementIgnoresLifecycle,
}

/// Remaining adversarial budget; the only unbounded resource of the
/// environment made finite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Budgets {
    /// Peers that may still go down.
    pub(super) departures: u8,
    /// Departed peers that may still start again.
    pub(super) rejoins: u8,
    /// Links that may still die between two live peers.
    pub(super) cuts: u8,
    /// Frames the network may still drop.
    pub(super) loss: u8,
    /// Frames the network may still deliver twice.
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
            (self.greater.peer, self.lesser),
            (self.lesser.peer, self.greater),
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
        /// Offerer's generation; `offered.peer` is the addressee.
        offered: PendingConnectionAttempt,
    },
    /// A frame on a link, tagged with the receiver-local generation `via` it
    /// arrives on; `via.peer` is the sender.
    Frame {
        /// Addressee.
        to: Did,
        /// Receiver's generation of the carrying link.
        via: PendingConnectionAttempt,
        /// Payload.
        frame: Frame,
    },
}

impl Envelope {
    /// The peer that would process this envelope.
    fn addressee(&self) -> Did {
        match self {
            Self::Offer { offered, .. } => offered.peer,
            Self::Frame { to, .. } => *to,
        }
    }

    /// Whether this envelope belongs to a stabilization round: its query,
    /// its report, or the notification its commit emits.
    const fn carries_stabilization_round(&self) -> bool {
        matches!(self, Self::Frame { .. })
    }

    /// The stabilization token this envelope could still echo to `requester`.
    fn echoes_token_of(&self, requester: Did) -> Option<uuid::Uuid> {
        match self {
            Self::Frame {
                via,
                frame: Frame::TopologyQuery { request_id },
                ..
            } if via.peer == requester => Some(*request_id),
            Self::Frame {
                to,
                frame: Frame::TopologyReport { request_id, .. },
                ..
            } if *to == requester => Some(*request_id),
            Self::Offer { .. } | Self::Frame { .. } => None,
        }
    }
}

/// A retired generation's event that changed protected state: the witness of
/// a violated exact-generation guard, recorded as a history variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct StaleEffect {
    /// Peer whose protected state changed.
    pub(super) node: Did,
    /// The retired generation the event was bound to.
    pub(super) retired: PendingConnectionAttempt,
}

/// The carrier of the model.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct OverlayState {
    /// Peers that are up.
    pub(super) nodes: BTreeMap<Did, NodeState>,
    /// Physical links that are alive.
    pub(super) links: BTreeSet<Link>,
    /// Envelopes in flight.
    pub(super) network: BTreeSet<Envelope>,
    /// Remaining adversarial budget.
    pub(super) remaining: Budgets,
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
    /// The network drops one frame.
    Lose(Envelope),
    /// The network delivers one frame and keeps a copy.
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
    /// A peer's shell delivers one local callback.
    Observe {
        /// Peer whose callback fires.
        node: Did,
        /// The callback.
        callback: Callback,
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
#[derive(Clone, Debug)]
pub(super) struct Overlay {
    /// Every modeled identity, evenly spaced on `Z/2^160` from the origin.
    ring: Vec<Did>,
    /// Successor-list capacity given to `topology::step`.
    successor_capacity: usize,
    /// Width of each modeled finger table.
    finger_slots: usize,
    /// Initial adversarial budget.
    budgets: Budgets,
    /// Shell defect under test, `Faithful` for the verified model.
    mutation: ShellMutation,
}

impl Overlay {
    /// A ring of `peers` identities at `origin + i · 2^160 / peers`.
    ///
    /// Even spacing makes every finger threshold `2^i` below the modeled
    /// width smaller than any inter-peer distance when the table is narrow,
    /// so each narrow finger's fixpoint is the successor head: the part of
    /// the finger table stabilization itself proves. Remote finger ranges
    /// belong to the finger-convergence model (Stage 5). The protocol is
    /// equivariant under rotation, so `origin` only matters when the ring has
    /// to contain a given production identity.
    pub(super) fn new(
        origin: Did,
        peers: u32,
        successor_capacity: usize,
        finger_slots: usize,
        budgets: Budgets,
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
            budgets,
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
                .map(|peer| (*peer, NodeState::started(*peer, self)))
                .collect(),
            links: BTreeSet::new(),
            network: BTreeSet::new(),
            remaining: self.budgets,
            stale_effect: None,
        };
        let pairs = self
            .ring
            .iter()
            .flat_map(|from| self.ring.iter().map(move |to| (*from, *to)))
            .filter(|(from, to)| from < to);
        for (from, to) in pairs {
            state = self.settled(self.transition(&state, from, |node| node.dial(to)));
        }
        for peer in self.ring.iter().copied() {
            let round = self.next_state(&state, OverlayAction::Stabilize(peer));
            state = self.settled(round.unwrap_or(state));
        }
        state
    }

    /// Deliver every envelope and callback until none is left, in
    /// action order: a deterministic churn-free schedule, used only to build
    /// `Init`.
    fn settled(&self, mut state: OverlayState) -> OverlayState {
        loop {
            let mut enabled = Vec::new();
            self.actions(&state, &mut enabled);
            let Some(next) = enabled
                .into_iter()
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

    /// Apply a node transition at `peer` and route its effects.
    ///
    /// This is the one place a node's state is replaced, so it is also the
    /// one place the retired-generation law is observed (see
    /// [`Self::transition_bound_to`]).
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
        let NodeStep { node, effects } = apply(node);
        next.nodes.insert(peer, node);
        for effect in effects {
            next.route(peer, effect);
        }
        next
    }

    /// [`Self::transition`] for an input bound to generation `bound`.
    ///
    /// Law (retired generations are inert): `¬Owns(bound) ⇒ protected' =
    /// protected`. A violation is recorded in the history variable rather than
    /// asserted, so the checker reports it with a minimal trace.
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
                && next
                    .nodes
                    .get(&peer)
                    .is_some_and(|after| after.protected() != before.protected())
        });
        if changed_by_retired {
            next.stale_effect = Some(StaleEffect {
                node: peer,
                retired: bound,
            });
        }
        next
    }

    /// The least token no in-flight artifact of `requester` can echo: the
    /// ascending fold skips exactly the initial run `1, 2, …` that is in use.
    ///
    /// Production draws a random UUID; only equality with outstanding tokens
    /// is observable, so the least unused value is a canonical representative
    /// that keeps the carrier finite.
    fn fresh_request_id(state: &OverlayState, requester: Did) -> uuid::Uuid {
        let least_unused = state
            .network
            .iter()
            .filter_map(|envelope| envelope.echoes_token_of(requester))
            .map(|token| token.as_u128())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .fold(1u128, |least, used| least + u128::from(used == least));
        uuid::Uuid::from_u128(least_unused)
    }

    /// Whether `peer` may start a maintenance period: it has a head to query
    /// and no stabilization round is in progress anywhere in the overlay.
    ///
    /// One round at a time is a bound of the search, not of the protocol:
    /// rounds of different peers touch disjoint topology states and read each
    /// other only through a report, whose content the claim-to-commit window
    /// does not change, so their interleavings add states without adding
    /// lifecycle races. Every lifecycle event still interleaves with the
    /// round. The bound also makes the token space finite; a lost query
    /// leaves nothing in flight, so the next period supersedes it as
    /// production does.
    fn may_stabilize(state: &OverlayState, node: &NodeState) -> bool {
        successor_head(&node.topology).is_some()
            && !state
                .network
                .iter()
                .any(Envelope::carries_stabilization_round)
    }

    /// Deliver `envelope` to its addressee; `None` when the environment has
    /// no budget for the refusal it would cause.
    fn delivered(&self, state: &OverlayState, envelope: Envelope) -> Option<OverlayState> {
        match envelope {
            Envelope::Offer { offerer, offered } => self.offer_delivered(state, offerer, offered),
            Envelope::Frame { to, via, frame } => {
                Some(
                    self.transition_bound_to(state, to, via, |node| node.receive(via, frame, self)),
                )
            }
        }
    }

    /// The addressee answers or refuses an offer.
    ///
    /// An offer *stands* while its offerer is up and still owns the offered
    /// generation; one that does not is void, and signaling drops it without
    /// consulting the addressee.
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
        let mut answered = None;
        let mut next = self.transition(state, offered.peer, |node| {
            let (node, answer) = node.answer_offer(offerer, self);
            answered = answer;
            NodeStep {
                node,
                effects: Vec::new(),
            }
        });
        match answered {
            Some(answer) => {
                next.links.insert(Link::pairing(offered, answer));
                next.raise(offerer, Callback::ChannelOpened(offered));
                next.raise(offered.peer, Callback::ChannelOpened(answer));
            }
            None => {
                next.remaining.refusal = next.remaining.refusal.checked_sub(1)?;
                next.raise(offerer, Callback::Closed(offered));
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

    /// Whether the link whose end `near` is held by `holder` is alive.
    pub(super) fn is_linked(&self, holder: Did, near: PendingConnectionAttempt) -> bool {
        self.links
            .iter()
            .any(|link| link.far_end(holder, near).is_some())
    }

    /// Queue `callback` at `peer`, if it is up.
    fn raise(&mut self, peer: Did, callback: Callback) {
        if let Some(node) = self.nodes.get_mut(&peer) {
            node.callbacks.insert(callback);
        }
    }

    /// Route one effect of `sender`.
    ///
    /// A frame sent under a generation whose link is already dead is lost: the
    /// sender has not yet observed the failure that the raised callbacks will
    /// report.
    fn route(&mut self, sender: Did, effect: Effect) {
        match effect {
            Effect::Offer { offered } => {
                self.network.insert(Envelope::Offer {
                    offerer: sender,
                    offered,
                });
            }
            Effect::Frame { under, frame } => {
                let via = self
                    .links
                    .iter()
                    .find_map(|link| link.far_end(sender, under));
                if let Some(via) = via {
                    self.network.insert(Envelope::Frame {
                        to: under.peer,
                        via,
                        frame,
                    });
                }
            }
        }
    }

    /// A link dies: both holders will observe a send failure and a close of
    /// exactly the generation that carried it.
    fn sever(&mut self, link: Link) {
        self.links.remove(&link);
        for (holder, end) in link.incidences() {
            self.raise(holder, Callback::SendTerminal(end));
            self.raise(holder, Callback::Closed(end));
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
            self.sever(link);
        }
        for node in self.nodes.values_mut() {
            if let Some(unadmitted) = node.lifecycles.unadmitted_attempt(peer) {
                node.callbacks.insert(Callback::Closed(unadmitted));
            }
        }
        self.network.retain(|envelope| {
            envelope.addressee() != peer
                && !matches!(envelope, Envelope::Offer { offerer, .. } if *offerer == peer)
        });
    }
}

impl Model for Overlay {
    type State = OverlayState;
    type Action = OverlayAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![self.converged_mesh()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.stale_effect.is_some() {
            // A recorded violation is terminal: the trace ends at its witness.
            return;
        }
        if state.remaining.departures > 0 {
            actions.extend(state.nodes.keys().copied().map(OverlayAction::Depart));
        }
        if state.remaining.rejoins > 0 {
            actions.extend(
                self.ring
                    .iter()
                    .copied()
                    .filter(|peer| !state.nodes.contains_key(peer))
                    .map(OverlayAction::Rejoin),
            );
        }
        if state.remaining.cuts > 0 {
            actions.extend(state.links.iter().copied().map(OverlayAction::Cut));
        }
        for envelope in state.network.iter() {
            let Some(addressee) = state.nodes.get(&envelope.addressee()) else {
                continue;
            };
            match envelope {
                // `next_state` disables an offer whose refusal is unbudgeted.
                Envelope::Offer { .. } => actions.push(OverlayAction::Deliver(envelope.clone())),
                Envelope::Frame { via, .. } if addressee.holds_before_admission(*via) => {}
                Envelope::Frame { .. } => {
                    actions.push(OverlayAction::Deliver(envelope.clone()));
                    if state.remaining.loss > 0 {
                        actions.push(OverlayAction::Lose(envelope.clone()));
                    }
                    if state.remaining.duplication > 0 {
                        actions.push(OverlayAction::Duplicate(envelope.clone()));
                    }
                }
            }
        }
        for (peer, node) in state.nodes.iter() {
            actions.extend(
                node.callbacks
                    .iter()
                    .map(|callback| OverlayAction::Observe {
                        node: *peer,
                        callback: *callback,
                    }),
            );
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
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let next = match action {
            OverlayAction::Depart(peer) => {
                let mut next = state.clone();
                next.remaining.departures = next.remaining.departures.checked_sub(1)?;
                next.depart(peer);
                next
            }
            OverlayAction::Rejoin(peer) => {
                let mut next = state.clone();
                next.remaining.rejoins = next.remaining.rejoins.checked_sub(1)?;
                next.nodes.insert(peer, NodeState::started(peer, self));
                next
            }
            OverlayAction::Cut(link) => {
                let mut next = state.clone();
                next.remaining.cuts = next.remaining.cuts.checked_sub(1)?;
                next.sever(link);
                next
            }
            OverlayAction::Lose(envelope) => {
                let mut next = state.clone();
                next.remaining.loss = next.remaining.loss.checked_sub(1)?;
                next.network.remove(&envelope);
                next
            }
            OverlayAction::Duplicate(envelope) => {
                let mut next = self.delivered(state, envelope)?;
                next.remaining.duplication = next.remaining.duplication.checked_sub(1)?;
                next
            }
            OverlayAction::Deliver(envelope) => {
                let mut next = self.delivered(state, envelope.clone())?;
                next.network.remove(&envelope);
                next
            }
            OverlayAction::Dial { from, to } => self.transition(state, from, |node| node.dial(to)),
            OverlayAction::Observe { node, callback } => {
                self.transition_bound_to(state, node, callback.attempt(), |current| {
                    current.observe(callback, self)
                })
            }
            OverlayAction::Stabilize(peer) => {
                let request_id = Self::fresh_request_id(state, peer);
                self.transition(state, peer, |node| node.stabilize(request_id, self))
            }
        };
        (next != *state).then_some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        laws::properties()
    }
}
