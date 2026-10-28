//! The holder of a relay inbox key: which node keeps the mail held for an offline peer, and
//! the members the mail is replicated to down a chain (`members`, #777).
//!
//! A relay carrier lives in a retained store only (#1021), and a node built or run without one
//! declares, in its connection handshake, that it holds no relay mail ([`RelayCustody`]). Such a
//! node still joins the ring, routes, and owns ordinary DHT data; only relay custody moves past
//! it, to the next node along the successor path that holds relay mail.
//!
//! Model. A node `n` owns the ring positions `(n, succ(n)]`, and `holds(n)` is its declaration.
//!
//! ```text
//! Law (Holder)    holder(k) = first n in [owner(k), succ(owner(k)), succ²(owner(k)), …]
//!                             with holds(n)
//! Law (Bound)     the search stops at the end of the successor list the deciding view knows:
//!                 past it the key has no holder (`Unheld`), and a hold is refused to its sender
//! Law (Agreement) every relay path (hold, removal, hand-off, repair, drain, fetch) decides with
//!                 `relay_holder`, so a writer and a reader whose views cover the same nodes name
//!                 the same holder
//! Law (Declared)  a peer's declaration arrives in the connection handshake, before the peer is
//!                 admitted; so every predecessor and successor a view places is placed with its
//!                 declaration, and only a previous holder named by a record is presumed to hold
//! Law (Previous holders) each repair pass a node passes to its successor head the list of its
//!                 nearest previous holders, at most r, nearest first: itself
//!                 and the nearer ones its predecessor passed it when it holds relay mail, the
//!                 predecessor's list otherwise; a list that comes round the ring names every
//!                 holder, and the view then searches round the ring for holders
//! Law (Convergence) after one membership change, the previous-holder lists settle within
//!                 r(L + 1) repair passes, L the length of the longest storage-less run and r the
//!                 replicas (L + 1 for the nearest previous holder alone); until then
//!                 a hold either lands where the repair passes move it on to the holder, or is
//!                 refused, and its writer, which keeps every hold until a holder acknowledges
//!                 it (`Law (Acknowledged)`), sends it again each pass until a holder takes it
//!                 or its sender proof expires
//! Law (Acknowledged) a writer keeps each hold from before its first send until the holder it
//!                 was last sent to acknowledges it, or its sender proof expires; it keeps at most
//!                 `UNACKNOWLEDGED_RELAY_HOLDS_MAX` and refuses another to its local sender
//!                 (`message::handlers::inbox::send_relay_hold`)
//! Law (Owner places) the owner of a key places it, as the key lies in its own arc, so its view
//!                 names the key's holder or none and never finds the owner `Unknown`; a keeper
//!                 whose view does not place a carrier's key asks the owner (`FindRelayHolder`)
//!                 and offers the carrier to the holder it names (`owner_answer`); it asks at
//!                 most one query per kept key whose owner its view does not place, per repair
//!                 pass, and takes only the first answer to a query it asked (`Law (Asked)`)
//! ```
//!
//! Law (Owner places) is what makes the convergence law hold for overlapping changes too: a
//! carrier whose holder lies behind several holders that joined within one window, out of reach
//! of both ends of the old holder's view, still reaches the holder its owner names (#1045).
//!
//! A view covers a clockwise `RelaySegment`: the nodes behind this one that it can place, this
//! node, and its successor list; a node with no successor owns the rest of the ring, as the
//! topology's successor lookup answers, and its segment closes the ring. Behind it, a node sees
//! its predecessor and its previous holders (`PreviousHolder`, `Law (Previous holders)`): the
//! nearest nodes behind it that hold relay mail, each together with its successor head, the
//! first node of the storage-less run after it. A node takes the list from its predecessor
//! alone. By induction along each run, every node of a storage-less run, and the holder that
//! ends it, learns the previous holders, however long the runs are. Every node of a run holds no
//! relay mail, so the run stands in the segment as one storage-less node, and a holder places
//! every key whose owner lies on a run behind one of its previous holders, which is what a
//! member decides its membership on (Lemma 2 of durable-records).
//!
//! ```text
//!   b ──▶ x ──▶ c ──▶ y ──▶ … ──▶ p ──▶ n ──▶ s₁ ──▶ … ──▶ s_r   b, c, n hold; x, y … p hold none
//!   passed head-ward:  c ─[(c, y), (b, x)]▶ y ─ … ─▶ p ─[(c, y), (b, x)]▶ n
//!
//!   segment(n) = [b ✓, x ✗, c ✓, y ✗, n ✓, s₁ …, s_r]   owner(k) = the segment node whose arc
//!                                                           holds k
//! ```

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::topology;
use super::topology::FindSuccessorStep;
use super::topology::TopologyState;
use super::Did;
use crate::consts::DEFAULT_RELAY_AGE_GATE_MS;
use crate::consts::DEFAULT_RELAY_INBOX_REPLICAS;
use crate::error::Error;

/// What a node declares about relay custody when it connects, and in its online descriptor:
/// whether it keeps a retained relay store and so holds relay mail.
///
/// The default is [`Self::Holds`], the presumption for a node not yet heard from.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub enum RelayCustody {
    /// The node keeps a retained relay store: it holds relay mail.
    #[default]
    Holds,
    /// The node keeps no relay store: it holds no relay mail, and custody moves past it.
    HoldsNone,
}

impl RelayCustody {
    /// The declaration of a node that does (`true`) or does not keep a relay store.
    pub const fn of(holds: bool) -> Self {
        if holds {
            Self::Holds
        } else {
            Self::HoldsNone
        }
    }

    /// Whether the declaring node holds relay mail.
    pub const fn holds(self) -> bool {
        matches!(self, Self::Holds)
    }
}

/// How relay custody is replicated (#777, durable-records Section X): the members each carrier
/// is replicated to, and how long a node is online before it declares that it holds relay mail
/// (`Law (Age gate)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayReplication {
    /// The members `r` a carrier is replicated to: the first `r` nodes from its key's owner
    /// that hold relay mail. At least one.
    pub replicas: usize,
    /// How long a node with a relay store is online before it declares that it holds relay
    /// mail; until then it routes and fetches its own mail, but is no member.
    pub age_gate_ms: u64,
}

impl Default for RelayReplication {
    /// [`DEFAULT_RELAY_INBOX_REPLICAS`] members, after [`DEFAULT_RELAY_AGE_GATE_MS`] online.
    fn default() -> Self {
        Self {
            replicas: DEFAULT_RELAY_INBOX_REPLICAS,
            age_gate_ms: DEFAULT_RELAY_AGE_GATE_MS,
        }
    }
}

/// The on-time probe answers in a row that clear a suspected chain neighbour (#1070, `Law
/// (Clearing)`). With the chain period `P_c` and timeout `τ_c`, a neighbour whose answers
/// hover around `τ_c` changes the chains at most twice, one suspicion and one clearing, per
/// `τ_c + k·P_c` (12.5 s at `k = 2`): the late probe takes `τ_c`, and the `k` on-time probes
/// that clear it are issued a period apart, the first no earlier than that late answer.
pub(crate) const CHAIN_NEIGHBOUR_CLEARING_ANSWERS: u8 = 2;

/// One node of a [`RelaySegment`] and its declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RelayNode {
    /// The node.
    pub(crate) did: Did,
    /// Whether it holds relay mail.
    pub(crate) holds: bool,
}

/// A clockwise run of consecutive ring nodes as one view knows them: node `i` owns the arc
/// `(nodes[i], nodes[i + 1]]`, and the last node's arc ends where the view ends, or, when the
/// segment closes the ring, at the first node.
///
/// Invariant: `nodes` is clockwise from its first node. A node may recur when a small ring's
/// successor list wraps past the start; the first arc containing a key decides, and a recurring
/// node owns the same arc each time, so recurrence changes no answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelaySegment {
    /// The nodes, clockwise.
    nodes: Vec<RelayNode>,
    /// Whether the segment closes the ring: its last node, which has no successor, owns every
    /// position up to the first node, as the topology's successor lookup answers, and a node
    /// alone owns every position.
    closes: bool,
    /// Whether the view knows every node of the ring that holds relay mail: its previous
    /// holders came round the ring to it (`Law (Previous holders)`). A search for holders that
    /// reaches the end of the view then continues from its start, where the rest of them are.
    knows_every_holder: bool,
}

impl RelaySegment {
    /// The segment of `nodes`, clockwise and open past its last node.
    pub(crate) fn new(nodes: Vec<RelayNode>) -> Self {
        Self {
            nodes,
            closes: false,
            knows_every_holder: false,
        }
    }

    /// The segment of `nodes`, clockwise, whose last node has no successor and owns the rest of
    /// the ring up to the first node.
    pub(crate) fn closing(nodes: Vec<RelayNode>) -> Self {
        Self {
            nodes,
            closes: true,
            knows_every_holder: false,
        }
    }

    /// This segment, whose view knows every node of the ring that holds relay mail.
    pub(crate) fn knowing_every_holder(self) -> Self {
        Self {
            knows_every_holder: true,
            ..self
        }
    }

    /// The index of the node owning `key`, if the segment places it.
    fn owner_index(&self, key: Did) -> Option<usize> {
        let open = self
            .nodes
            .windows(2)
            .position(|arc| matches!(arc, [from, to] if in_arc(from.did, key, to.did)));
        match (open, self.nodes.first(), self.nodes.last()) {
            (Some(owner), _, _) => Some(owner),
            (None, Some(first), Some(last))
                if self.closes && (self.nodes.len() == 1 || in_arc(last.did, key, first.did)) =>
            {
                Some(self.nodes.len() - 1)
            }
            _ => None,
        }
    }

    /// The nodes from index `owner` on, clockwise: to the end of the view, and round to the
    /// start when the segment closes the ring or knows every holder.
    fn clockwise_from(&self, owner: usize) -> impl Iterator<Item = &RelayNode> {
        let wrapped = if self.closes || self.knows_every_holder {
            owner
        } else {
            0
        };
        self.nodes
            .iter()
            .skip(owner)
            .chain(self.nodes.iter().take(wrapped))
    }
}

/// `key ∈ (from, to]`, clockwise.
pub(crate) fn in_arc(from: Did, key: Did, to: Did) -> bool {
    key != from && Did::cmp_from_observer(from, key, to) != Ordering::Greater
}

/// Whether `error` says a relay hold had no effect and may take effect later, so its writer
/// keeps it and sends it again on its next repair pass: a view that names no holder, a target
/// whose view does not make it the holder or does not admit the writer, a target without a relay
/// store, or a send refused before acceptance. Views disagree only until they converge
/// (`Law (Convergence)`), so a kept hold reaches its holder unless its sender proof expires first.
///
/// ```text
/// Law: is_unplaced_hold(e) ⟺ e.is_view_refusal() ∨ e = SingleAttemptRefused
/// ```
pub(crate) fn is_unplaced_hold(error: &Error) -> bool {
    error.is_view_refusal() || matches!(error, Error::SingleAttemptRefused { .. })
}

/// The holder a view names for a relay key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayHolder {
    /// This node holds the key's mail.
    Holder(Did),
    /// The view places the key's owner, and no node from it to the end of the view holds relay
    /// mail (`Law (Bound)`): the mail has no holder, and a hold is refused.
    Unheld,
    /// The view does not place the key's owner: this node can name no holder.
    Unknown,
}

/// The holder of `key` under `segment` (`Law (Holder)`, `Law (Bound)`): the first node at or
/// after the key's owner that holds relay mail.
///
/// Post: `Holder(h)` implies `h` is in `segment` and holds relay mail, and every node of the
/// segment from the owner up to `h` holds none; `Unheld` implies the owner is placed and no node
/// from it on holds relay mail; `Unknown` implies the segment places no owner of `key`.
pub(crate) fn relay_holder(segment: &RelaySegment, key: Did) -> RelayHolder {
    let Some(owner) = segment.owner_index(key) else {
        return RelayHolder::Unknown;
    };
    segment
        .clockwise_from(owner)
        .find(|node| node.holds)
        .map_or(RelayHolder::Unheld, |node| RelayHolder::Holder(node.did))
}

/// The holder the owner of `key` names to a keeper that asks (`FindRelayHolder`): the holder its
/// own `segment` names. The owner places every key of its own arc, so its answer is a holder or
/// `None` for a key without one, and `None` too when this node does not own `key`.
///
/// Law (Owner places): a node that owns `key` (`key ∈ (n, succ(n)]`, its segment's own arc)
/// never finds its owner `Unknown`: `owner_answer` is the holder the owner's view names.
pub(crate) fn owner_answer(segment: &RelaySegment, owner: Did, key: Did) -> Option<Did> {
    let owns = segment
        .owner_index(key)
        .and_then(|index| segment.nodes.get(index))
        .is_some_and(|node| node.did == owner);
    match relay_holder(segment, key) {
        RelayHolder::Holder(holder) if owns => Some(holder),
        _ => None,
    }
}

/// The nearest node behind this one that holds relay mail, and the head of its successor list:
/// the first node of the storage-less run between them, or this node when there is none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviousHolder {
    /// The previous holder.
    pub(crate) holder: Did,
    /// Its successor head.
    pub(crate) run_start: Did,
}

/// The previous holders as the predecessor `from` passed them.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Passed {
    /// The predecessor that passed the record.
    from: Did,
    /// The record: the nearest previous holder first, at most `r`.
    previous: Vec<PreviousHolder>,
    /// Whether the record came round the ring to the receiver, so that it names every node of
    /// the ring that holds relay mail but the receiver.
    closed: bool,
}

/// Who may hold a message for a destination, as the holder of its inbox key sees it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HoldAuthority {
    /// Exactly this node is responsible for the destination.
    Node(Did),
    /// The responsible node is the first node at or after the destination, and the view knows
    /// only that it lies on the storage-less run in `[first, last]`.
    Run {
        /// The destination.
        first: Did,
        /// The predecessor of the holder, the last node of the run.
        last: Did,
    },
    /// The view does not say who is responsible: no writer is admitted.
    Unknown,
}

impl HoldAuthority {
    /// Whether `writer` may hold for the destination, `holds` being the view's declarations.
    pub(crate) fn admits(self, writer: Did, holds: impl Fn(Did) -> bool) -> bool {
        match self {
            Self::Node(responsible) => responsible == writer,
            Self::Run { first, last } => {
                (writer == first || in_arc(first, writer, last)) && !holds(writer)
            }
            Self::Unknown => false,
        }
    }
}

/// The relay custody this node knows of its neighbours: the peers that declared they hold no
/// relay mail, and the previous holders its predecessor passed it.
///
/// A peer is presumed to hold relay mail until it declares otherwise, as every node did before
/// the declaration existed, so a node not yet heard from is never skipped.
#[derive(Clone, Debug)]
pub(crate) struct RelayNeighborhood {
    /// Peers that declared [`RelayCustody::HoldsNone`].
    storage_less: BTreeSet<Did>,
    /// The previous holders the predecessor last passed.
    passed: Option<Passed>,
    /// The members a carrier is replicated to, `r`, and so the previous holders a list keeps.
    replicas: usize,
    /// Chain neighbours whose liveness probe went unanswered for the chain timeout (#1070), each
    /// with the on-time answers it gave since: no member of any chain until
    /// [`CHAIN_NEIGHBOUR_CLEARING_ANSWERS`] in a row. Suspicion moves the chain alone; the passed
    /// list, whose agreement reads declarations, is read as before.
    suspected: BTreeMap<Did, u8>,
}

impl Default for RelayNeighborhood {
    /// No declaration and no list yet, at [`DEFAULT_RELAY_INBOX_REPLICAS`].
    fn default() -> Self {
        Self::with_replicas(DEFAULT_RELAY_INBOX_REPLICAS)
    }
}

impl RelayNeighborhood {
    /// No declaration and no list yet, replicating to `replicas` members (at least one).
    pub(crate) fn with_replicas(replicas: usize) -> Self {
        Self {
            storage_less: BTreeSet::new(),
            passed: None,
            replicas: replicas.max(1),
            suspected: BTreeMap::new(),
        }
    }

    /// Whether `peer` is a candidate member of a chain: it holds relay mail as far as this node
    /// knows, and is not suspected (#1070).
    pub(crate) fn chains(&self, peer: Did) -> bool {
        self.holds(peer) && !self.suspected.contains_key(&peer)
    }

    /// Suspect `peer`, a chain neighbour that has not answered its probe (#1070), with no
    /// on-time answer since. Returns whether it was not suspected before: only that is a chain
    /// change.
    pub(crate) fn suspect(&mut self, peer: Did) -> bool {
        match self.suspected.get_mut(&peer) {
            Some(on_time) => {
                *on_time = 0;
                false
            }
            None => {
                self.suspected.insert(peer, 0);
                true
            }
        }
    }

    /// The suspected chain neighbours, which stay on the chain period until cleared (#1070).
    pub(crate) fn suspects(&self) -> impl Iterator<Item = Did> + '_ {
        self.suspected.keys().copied()
    }

    /// Record `peer`'s answer to its probe, `on_time` when it came within the chain timeout
    /// (`Law (Clearing)`): a suspect is cleared by [`CHAIN_NEIGHBOUR_CLEARING_ANSWERS`] on-time
    /// answers in a row, and a late answer starts the count again. Returns whether the
    /// suspicion was cleared: only that is a chain change.
    pub(crate) fn observe_answer(&mut self, peer: Did, on_time: bool) -> bool {
        let Some(answers) = self.suspected.get_mut(&peer) else {
            return false;
        };
        *answers = if on_time {
            answers.saturating_add(1)
        } else {
            0
        };
        if *answers < CHAIN_NEIGHBOUR_CLEARING_ANSWERS {
            return false;
        }
        self.suspected.remove(&peer);
        true
    }

    /// Whether `peer` holds relay mail, as far as this node knows.
    pub(crate) fn holds(&self, peer: Did) -> bool {
        !self.storage_less.contains(&peer)
    }

    /// Record the declaration `peer` made when it connected. A previous holder that now declares
    /// it holds no relay mail is no previous holder.
    pub(crate) fn observe(&mut self, peer: Did, custody: RelayCustody) {
        if custody.holds() {
            self.storage_less.remove(&peer);
        } else {
            self.storage_less.insert(peer);
            self.forget_previous_holder(peer);
        }
    }

    /// Forget `peer`, which left this node's topology, with any record it passed or names.
    pub(crate) fn forget(&mut self, peer: Did) {
        self.storage_less.remove(&peer);
        self.suspected.remove(&peer);
        self.passed = self.passed.take().filter(|passed| passed.from != peer);
        self.forget_previous_holder(peer);
    }

    /// Cut the passed record before `peer`, which is no previous holder (any more): the holders
    /// nearer than it are still the nearest ones, and the next pass refills the rest.
    fn forget_previous_holder(&mut self, peer: Did) {
        if let Some(passed) = self.passed.as_mut() {
            if let Some(at) = passed
                .previous
                .iter()
                .position(|known| known.holder == peer)
            {
                passed.previous.truncate(at);
                passed.closed = false;
            }
        }
        self.passed = self
            .passed
            .take()
            .filter(|passed| !passed.previous.is_empty());
    }

    /// The peers that declared they hold no relay mail.
    pub(crate) fn storage_less_peers(&self) -> Vec<Did> {
        self.storage_less.iter().copied().collect()
    }

    /// Keep only the declarations of the peers `keep` names, bounding the record by them.
    pub(crate) fn retain(&mut self, keep: impl Fn(Did) -> bool) {
        self.storage_less.retain(|peer| keep(*peer));
    }

    /// Take the previous holders `previous` that `from` passed to `local`, nearest first, iff
    /// `from` is the predecessor of `state`, the nearest names a node that may hold relay mail,
    /// and its run starts after it and ends at `local` (`Law (Previous holders)`). A record
    /// naming `local` itself has come round the whole ring: `local` is the only holder, and the
    /// run is every other node. Of the farther holders, the longest prefix that is a chain is
    /// kept ([`Self::chain`]).
    ///
    /// Post: the result says whether the record was taken; it replaces the one taken before.
    pub(crate) fn pass(
        &mut self,
        state: &TopologyState,
        from: Did,
        previous: Vec<PreviousHolder>,
    ) -> bool {
        let Some(nearest) = previous.first().copied() else {
            return false;
        };
        let run_ends_here = if nearest.holder == state.local {
            nearest.run_start != state.local
        } else {
            in_arc(nearest.holder, nearest.run_start, state.local)
        };
        let taken = state.predecessor == Some(from) && self.holds(nearest.holder) && run_ends_here;
        if taken {
            let (previous, closed) = self.chain(state.local, previous);
            self.passed = Some(Passed {
                from,
                previous,
                closed,
            });
        }
        taken
    }

    /// The longest prefix of `previous` that is a chain of previous holders behind `local`: at
    /// most `r` distinct holders, each presumed to hold relay mail, each one
    /// behind the one before it with its run between them (`run_start ∈ (holder, nearer]`), and
    /// none past `local` itself, where a record that came round the ring closes; and whether it
    /// did close, naming every holder of the ring but `local`.
    ///
    /// Pre: `previous[0]` was checked by [`Self::pass`].
    fn chain(&self, local: Did, previous: Vec<PreviousHolder>) -> (Vec<PreviousHolder>, bool) {
        let mut chain: Vec<PreviousHolder> = Vec::with_capacity(self.replicas);
        let mut closed = false;
        for link in previous.into_iter().take(self.replicas) {
            if link.holder == local {
                closed = true;
                if chain.is_empty() {
                    chain.push(link);
                }
                break;
            }
            let follows = match chain.last() {
                None => true,
                Some(nearer) => {
                    self.holds(link.holder)
                        && in_arc(link.holder, link.run_start, nearer.holder)
                        && chain.iter().all(|known| known.holder != link.holder)
                }
            };
            if !follows {
                break;
            }
            chain.push(link);
        }
        (chain, closed)
    }

    /// The previous holders, nearest first, iff the current predecessor passed them and the
    /// nearest agrees with that predecessor: one that holds relay mail is the nearest previous
    /// holder itself, with this node as its head, and a storage-less one lies on its run.
    fn previous_holders_in(&self, state: &TopologyState) -> Option<&Passed> {
        let passed = self.passed.as_ref()?;
        let predecessor = state
            .predecessor
            .filter(|predecessor| *predecessor == passed.from)?;
        let nearest = passed.previous.first()?;
        let agrees = if self.holds(predecessor) {
            predecessor == nearest.holder && nearest.run_start == state.local
        } else {
            predecessor == nearest.run_start || in_arc(nearest.run_start, predecessor, state.local)
        };
        agrees.then_some(passed)
    }

    /// The nearest node behind this one that is a candidate chain member, `p₁`: the nearest
    /// previous holder passed that is not suspected (#1070), or, before any list, the
    /// predecessor when it holds relay mail and is not.
    pub(crate) fn nearest_previous_holder(&self, state: &TopologyState) -> Option<Did> {
        match self.previous_holders_in(state) {
            Some(passed) => passed
                .previous
                .iter()
                .map(|link| link.holder)
                .take_while(|holder| *holder != state.local)
                .find(|holder| self.chains(*holder)),
            None => state
                .predecessor
                .filter(|predecessor| self.chains(*predecessor)),
        }
    }

    /// The segment the view `state` covers, this node holding relay mail iff `local_holds`.
    ///
    /// Behind this node it places its previous holders, farthest first, each followed by the
    /// first node of its storage-less run, which stands in for the whole run: so a holder
    /// places every key whose owner lies on a run behind one of its previous holders.
    pub(crate) fn segment(&self, state: &TopologyState, local_holds: bool) -> RelaySegment {
        let local = RelayNode {
            did: state.local,
            holds: local_holds,
        };
        let mut nodes = Vec::with_capacity(state.successors.len() + 2 * self.replicas + 1);
        let passed = self.previous_holders_in(state);
        match (
            passed.map(|passed| passed.previous.as_slice()),
            state.predecessor,
        ) {
            (Some(previous), _) => {
                for (at, link) in previous.iter().enumerate().rev() {
                    if link.holder != state.local {
                        nodes.push(RelayNode {
                            did: link.holder,
                            holds: self.chains(link.holder),
                        });
                    }
                    let after = at
                        .checked_sub(1)
                        .and_then(|nearer| previous.get(nearer))
                        .map_or(state.local, |nearer| nearer.holder);
                    if link.run_start != after {
                        nodes.push(RelayNode {
                            did: link.run_start,
                            holds: false,
                        });
                    }
                }
            }
            (None, Some(predecessor)) => nodes.push(self.node(predecessor)),
            (None, None) => {}
        }
        nodes.push(local);
        if state.successors.is_empty() {
            return RelaySegment::closing(nodes);
        }
        nodes.extend(
            state
                .successors
                .iter()
                .map(|successor| self.node(*successor)),
        );
        let segment = RelaySegment::new(nodes);
        // A list that came round the ring names every holder; a successor list that reaches the
        // predecessor covers every node of the ring.
        let whole_ring = passed.is_some_and(|passed| passed.closed)
            || state
                .predecessor
                .is_some_and(|predecessor| state.successors.contains(&predecessor));
        if whole_ring {
            return segment.knowing_every_holder();
        }
        segment
    }

    /// `peer` with its declaration, read as a candidate chain member (not suspected, #1070).
    fn node(&self, peer: Did) -> RelayNode {
        RelayNode {
            did: peer,
            holds: self.chains(peer),
        }
    }

    /// The record this node passes to its successor head (`Law (Previous holders)`): itself
    /// followed by the nearer previous holders it knows when it holds relay mail
    /// (`local_holds`), at most `r` in all, otherwise the previous holders
    /// its predecessor passed it. `None` without a head, or for a storage-less node that knows
    /// no previous holder.
    pub(crate) fn record_to_pass(
        &self,
        state: &TopologyState,
        local_holds: bool,
    ) -> Option<(Did, Vec<PreviousHolder>)> {
        let head = *state.successors.first()?;
        let known = self
            .previous_holders_in(state)
            .map(|passed| passed.previous.as_slice())
            .unwrap_or_default();
        let previous = if local_holds {
            let own = PreviousHolder {
                holder: state.local,
                run_start: head,
            };
            std::iter::once(own)
                .chain(
                    known
                        .iter()
                        .copied()
                        .take_while(|link| link.holder != state.local),
                )
                .take(self.replicas)
                .collect()
        } else if known.is_empty() {
            return None;
        } else {
            known.to_vec()
        };
        Some((head, previous))
    }

    /// Who may hold a message for `destination` at this node (see [`HoldAuthority`]).
    pub(crate) fn hold_authority(&self, state: &TopologyState, destination: Did) -> HoldAuthority {
        if let FindSuccessorStep::Local(responsible) = topology::find_successor(state, destination)
        {
            return HoldAuthority::Node(responsible);
        }
        if topology::is_responsible_for(state, destination) {
            return HoldAuthority::Node(state.local);
        }
        match (
            self.previous_holders_in(state)
                .and_then(|passed| passed.previous.first()),
            state.predecessor,
        ) {
            (Some(previous), Some(last))
                if !self.holds(last)
                    && (destination == previous.run_start
                        || in_arc(previous.run_start, destination, last)) =>
            {
                HoldAuthority::Run {
                    first: destination,
                    last,
                }
            }
            _ => HoldAuthority::Unknown,
        }
    }

    /// Whether this node takes a relay carrier relocated by `sender`: iff `sender` holds relay
    /// mail, the only kind of node that ever keeps a carrier. The receiver also takes only a key
    /// its own view makes it hold, and only elements that pass the witness.
    ///
    /// The sender need not be a neighbour. Custody moves to the holder the sender's view names,
    /// and the two need not know each other: a holder that gives back the keys of a run a
    /// relay-capable node joined, or a holder after a run longer than a successor list, sends
    /// to a node that sees neither it as predecessor nor as the next holder.
    pub(crate) fn relocates_from(&self, sender: Did) -> bool {
        self.holds(sender)
    }
}

mod members;
pub(crate) use members::chain_position;
pub(crate) use members::relay_carrier_offer;
pub(crate) use members::relay_members;
pub(crate) use members::CarrierOffer;
pub(crate) use members::ChainNext;
pub(crate) use members::ChainPosition;
pub(crate) use members::RelayMembers;

#[cfg(test)]
mod test_members;
#[cfg(test)]
mod tests;
