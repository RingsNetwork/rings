//! The holder of a relay inbox key: which node keeps the mail held for an offline peer.
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
//! Law (Convergence) after one membership change, the previous-holder records settle within
//!                 L + 1 repair passes, L the length of the longest storage-less run; until then
//!                 a hold either lands where the repair passes move it on to the holder, or is
//!                 refused, and its writer, which keeps every hold until a holder acknowledges
//!                 it (`Law (Acknowledged)`), sends it again each pass until a holder takes it
//!                 or its sender proof expires
//! Law (Acknowledged) a writer keeps each hold from before its first send until the holder it
//!                 was last sent to acknowledges it, or its sender proof expires; it keeps at most
//!                 `UNACKNOWLEDGED_RELAY_HOLDS_MAX` and refuses another to its local sender
//!                 (`message::handlers::inbox::send_relay_hold`)
//! ```
//!
//! The convergence law covers every single membership change and overlapping changes whose run
//! fits a successor list; two relay-capable nodes joining one longer run within one window can
//! strand a carrier at the old holder (#1045).
//!
//! A view covers a clockwise `RelaySegment`: the nodes behind this one that it can place, this
//! node, and its successor list; a node with no successor owns the rest of the ring, as the
//! topology's successor lookup answers, and its segment closes the ring. Behind it, a node sees
//! its predecessor and the previous holder (`PreviousHolder`): the nearest node behind it that
//! holds relay mail, together with that node's successor head, the first node of the
//! storage-less run between them. Every node passes the previous holder it knows to its own
//! successor head on each repair pass: a node that holds relay mail names itself, a node that
//! holds none passes on what its predecessor passed it, and a node takes the record from its
//! predecessor alone. By induction along the run, every node of a storage-less run, and the
//! holder that ends it, learns the run's previous holder, however long the run is. Every node of
//! the run holds no relay mail, so the run stands in the segment as one storage-less node, and a
//! holder places every key it holds even when the key's owner lies several storage-less hops
//! behind it.
//!
//! ```text
//!   c ──▶ y ──▶ … ──▶ p ──▶ n ──▶ s₁ ──▶ … ──▶ s_r     c, n hold relay mail; y … p hold none
//!   passed head-ward:  c ─(c, y)▶ y ─(c, y)▶ … ─(c, y)▶ p ─(c, y)▶ n
//!
//!   segment(n) = [c ✓, y ✗, n ✓, s₁ …, s_r]   owner(k) = the segment node whose arc holds k
//! ```

use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;

use super::topology;
use super::topology::FindSuccessorStep;
use super::topology::TopologyState;
use super::Did;
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
}

impl RelaySegment {
    /// The segment of `nodes`, clockwise and open past its last node.
    pub(crate) fn new(nodes: Vec<RelayNode>) -> Self {
        Self {
            nodes,
            closes: false,
        }
    }

    /// The segment of `nodes`, clockwise, whose last node has no successor and owns the rest of
    /// the ring up to the first node.
    pub(crate) fn closing(nodes: Vec<RelayNode>) -> Self {
        Self {
            nodes,
            closes: true,
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
    /// start when the segment closes the ring.
    fn clockwise_from(&self, owner: usize) -> impl Iterator<Item = &RelayNode> {
        let wrapped = if self.closes { owner } else { 0 };
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
pub(crate) fn is_unplaced_hold(error: &Error) -> bool {
    matches!(
        error,
        Error::RelayHolderUnavailable
            | Error::RelayCarrierNotHeldHere
            | Error::RelayMessageHolderNotResponsible
            | Error::RelayStorageUnavailable
            | Error::SingleAttemptRefused { .. }
    )
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

/// The nodes a keeper of the carrier of `key` offers it to, other than `local` itself: the
/// holder `segment` names; or, when the view does not place the key's owner, both ends of the
/// view that hold relay mail. The key then lies past the last node, toward which a holder's
/// interval moves when nodes join ahead, or behind the first, the previous holder, which takes
/// back the keys of a run a relay-capable node joined. Each end is the nearest node the view
/// knows on its side, and takes the carrier only if its own view names it the holder, so an
/// offer to the wrong side moves nothing. Empty: keep the carrier.
///
/// Post: every target is not `local` and holds relay mail, and there are at most two.
pub(crate) fn relay_handoff_targets(segment: &RelaySegment, key: Did, local: Did) -> Vec<Did> {
    let mut targets = match relay_holder(segment, key) {
        RelayHolder::Holder(holder) => vec![holder],
        RelayHolder::Unknown => [segment.nodes.last(), segment.nodes.first()]
            .into_iter()
            .flatten()
            .filter(|end| end.holds)
            .map(|end| end.did)
            .collect(),
        RelayHolder::Unheld => Vec::new(),
    };
    targets.dedup();
    targets.retain(|target| *target != local);
    targets
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

/// A previous holder as the predecessor `from` passed it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Passed {
    /// The predecessor that passed the record.
    from: Did,
    /// The record.
    previous: PreviousHolder,
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
/// relay mail, and the previous holder its predecessor passed it.
///
/// A peer is presumed to hold relay mail until it declares otherwise, as every node did before
/// the declaration existed, so a node not yet heard from is never skipped.
#[derive(Clone, Debug, Default)]
pub(crate) struct RelayNeighborhood {
    /// Peers that declared [`RelayCustody::HoldsNone`].
    storage_less: BTreeSet<Did>,
    /// The previous holder the predecessor last passed.
    passed: Option<Passed>,
}

impl RelayNeighborhood {
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
            self.passed = self.passed.filter(|passed| passed.previous.holder != peer);
        }
    }

    /// Forget `peer`, which left this node's topology, with any record it passed or names.
    pub(crate) fn forget(&mut self, peer: Did) {
        self.storage_less.remove(&peer);
        self.passed = self
            .passed
            .filter(|passed| passed.from != peer && passed.previous.holder != peer);
    }

    /// The peers that declared they hold no relay mail.
    pub(crate) fn storage_less_peers(&self) -> Vec<Did> {
        self.storage_less.iter().copied().collect()
    }

    /// Keep only the declarations of the peers `keep` names, bounding the record by them.
    pub(crate) fn retain(&mut self, keep: impl Fn(Did) -> bool) {
        self.storage_less.retain(|peer| keep(*peer));
    }

    /// Take the previous holder `previous` that `from` passed to `local`, iff `from` is the
    /// predecessor of `state`, `previous` names a node that may hold relay mail, and its run
    /// starts after it and ends at `local`. A record naming `local` itself has come round the
    /// whole ring: `local` is the only holder, and the run is every other node.
    ///
    /// Post: the result says whether the record was taken; it replaces the one taken before.
    pub(crate) fn pass(
        &mut self,
        state: &TopologyState,
        from: Did,
        previous: PreviousHolder,
    ) -> bool {
        let run_ends_here = if previous.holder == state.local {
            previous.run_start != state.local
        } else {
            in_arc(previous.holder, previous.run_start, state.local)
        };
        let taken = state.predecessor == Some(from) && self.holds(previous.holder) && run_ends_here;
        if taken {
            self.passed = Some(Passed { from, previous });
        }
        taken
    }

    /// The previous holder, iff the current predecessor passed it and it agrees with that
    /// predecessor: one that holds relay mail is the previous holder itself, with this node as
    /// its head, and a storage-less one lies on the run.
    fn previous_holder_in(&self, state: &TopologyState) -> Option<PreviousHolder> {
        let Passed { from, previous } = self.passed?;
        let predecessor = state
            .predecessor
            .filter(|predecessor| *predecessor == from)?;
        let agrees = if self.holds(predecessor) {
            predecessor == previous.holder && previous.run_start == state.local
        } else {
            predecessor == previous.run_start
                || in_arc(previous.run_start, predecessor, state.local)
        };
        agrees.then_some(previous)
    }

    /// The segment the view `state` covers, this node holding relay mail iff `local_holds`.
    pub(crate) fn segment(&self, state: &TopologyState, local_holds: bool) -> RelaySegment {
        let local = RelayNode {
            did: state.local,
            holds: local_holds,
        };
        let mut nodes = Vec::with_capacity(state.successors.len() + 3);
        match (self.previous_holder_in(state), state.predecessor) {
            (Some(previous), _) => {
                if previous.holder != state.local {
                    nodes.push(RelayNode {
                        did: previous.holder,
                        holds: true,
                    });
                }
                if previous.run_start != state.local {
                    nodes.push(RelayNode {
                        did: previous.run_start,
                        holds: false,
                    });
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
        RelaySegment::new(nodes)
    }

    /// `peer` with its declaration.
    fn node(&self, peer: Did) -> RelayNode {
        RelayNode {
            did: peer,
            holds: self.holds(peer),
        }
    }

    /// The record this node passes to its successor head: itself when it holds relay mail
    /// (`local_holds`), otherwise the previous holder its predecessor passed it. `None` without
    /// a head, or for a storage-less node that knows no previous holder.
    pub(crate) fn record_to_pass(
        &self,
        state: &TopologyState,
        local_holds: bool,
    ) -> Option<(Did, PreviousHolder)> {
        let head = *state.successors.first()?;
        let previous = if local_holds {
            PreviousHolder {
                holder: state.local,
                run_start: head,
            }
        } else {
            self.previous_holder_in(state)?
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
        match (self.previous_holder_in(state), state.predecessor) {
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

#[cfg(test)]
mod tests;
