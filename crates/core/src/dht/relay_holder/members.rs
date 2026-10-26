//! Replicated custody (#777; durable-records, Section X): the members of a relay key, and what a
//! keeper of a carrier does with it on a repair pass.
//!
//! ```text
//! Members          M_r(k) = the first r nodes of [owner(k), succ(owner(k)), …] that hold relay
//!                  mail; its first is the holder of `Law (Holder)`
//! Law (Members)    every node decides membership on its own segment, which places its r
//!                  previous holders behind it (`Law (Previous holders)`): m ∈ M_r(k) iff
//!                  k ∈ (succ(p_r(m)), succ(m)] (Lemma 2), so no lookup is needed
//! Law (Chain repair) a member's carriers share its chain neighbours p₁ and n₁, so a repair pass
//!                  offers one digest set to each (`message::handlers::relay_repair`); a keeper
//!                  that is no member offers its carrier to the members as an ownership hand-off,
//!                  and deletes its copy only after a member acknowledges holding it (S1'', S2')
//! ```
//!
//! At `r = 1` the members are the holder alone and an offer is the hand-off of `Law (Holder)`.

use super::RelaySegment;
use crate::dht::Did;
use crate::dht::StorageSyncPurpose;

/// The members a view names for a relay key (`Law (Members)`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RelayMembers {
    /// The members the view places, the holder first: at least one, at most the replicas asked
    /// for, fewer only where the view ends (`Law (Bound)`).
    Members(Vec<Did>),
    /// The view places the key's owner, and no node from it to the end of the view holds relay
    /// mail: the key has no member.
    Unheld,
    /// The view does not place the key's owner.
    Unknown,
}

impl RelayMembers {
    /// Whether `node` is a member.
    pub(crate) fn contains(&self, node: Did) -> bool {
        matches!(self, Self::Members(members) if members.contains(&node))
    }
}

/// The first `replicas` nodes of `segment` at or after the owner of `key` that hold relay mail
/// (`Law (Members)`).
///
/// Post: `Members(m)` implies `m[0]` is what `relay_holder` names, `1 <= m.len() <= replicas`
/// for `replicas >= 1`, and every member holds relay mail; `Unheld` and `Unknown` as
/// `relay_holder` answers them.
pub(crate) fn relay_members(segment: &RelaySegment, key: Did, replicas: usize) -> RelayMembers {
    let Some(owner) = segment.owner_index(key) else {
        return RelayMembers::Unknown;
    };
    let mut members = Vec::with_capacity(replicas);
    for node in segment.clockwise_from(owner).filter(|node| node.holds) {
        if members.len() == replicas {
            break;
        }
        // A small ring's successor list may wrap past the start: a node is one member.
        if !members.contains(&node.did) {
            members.push(node.did);
        }
    }
    if members.is_empty() {
        RelayMembers::Unheld
    } else {
        RelayMembers::Members(members)
    }
}

/// What a keeper does with a carrier on a repair pass (`Law (Chain repair)`): a member's copies
/// go through the chain's coalesced offers, so only a hand-off is sent per carrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CarrierOffer {
    /// How the carrier is offered: an additive copy between members, or an ownership hand-off
    /// from a keeper that is no member.
    pub(crate) purpose: StorageSyncPurpose,
    /// The nodes it is offered to, never the keeper itself; empty: keep it.
    pub(crate) targets: Vec<Did>,
}

/// The offer `local`, which keeps the carrier of `key`, makes under `segment`: to the other
/// members as copies when `local` is a member; to the members as a hand-off when it is not; and,
/// when the view does not place the key's owner, as a hand-off to the holder the owner named
/// (`owner_named`, `Law (Owner places)`) and to both ends of the view that hold relay mail. The
/// key then lies past the last node, toward which a holder's interval moves when nodes join
/// ahead, or behind the first, the farthest previous holder, which takes back the keys of a run
/// a relay-capable node joined; the owner's answer reaches a holder neither end knows (#1045).
/// Each target takes the carrier only if its own view makes it a member, so an offer to the
/// wrong side moves nothing.
///
/// Post: every target is not `local` and appears once; a hand-off has at most `replicas`
/// targets, or the owner's answer and two ends.
pub(crate) fn relay_carrier_offer(
    segment: &RelaySegment,
    key: Did,
    local: Did,
    owner_named: Option<Did>,
    replicas: usize,
) -> CarrierOffer {
    let (purpose, mut targets) = match relay_members(segment, key, replicas) {
        RelayMembers::Members(members) if members.contains(&local) => {
            (StorageSyncPurpose::AdditiveRepair, members)
        }
        RelayMembers::Members(members) => (StorageSyncPurpose::OwnershipHandoff, members),
        RelayMembers::Unknown => (
            StorageSyncPurpose::OwnershipHandoff,
            owner_named
                .into_iter()
                .chain(
                    [segment.nodes.last(), segment.nodes.first()]
                        .into_iter()
                        .flatten()
                        .filter(|end| end.holds)
                        .map(|end| end.did),
                )
                .collect(),
        ),
        RelayMembers::Unheld => (StorageSyncPurpose::OwnershipHandoff, Vec::new()),
    };
    let mut unique = Vec::with_capacity(targets.len());
    for target in targets.drain(..) {
        if target != local && !unique.contains(&target) {
            unique.push(target);
        }
    }
    CarrierOffer {
        purpose,
        targets: unique,
    }
}

/// Where a member's next member lies (`Law (Chain replication)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChainNext {
    /// The member is the last of the chain: its rank is `r − 1`, or no holder follows it on a
    /// ring whose every holder the view knows.
    End,
    /// The next member, which the view places.
    Member(Did),
    /// The next member lies past the view: it is `n₁`, the first holder after the member,
    /// resolved by the owner query (`Law (Owner places)`).
    BeyondView,
}

/// A member's place in the chain of a relay key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainPosition {
    /// The member's rank, 0 for the holder.
    pub(crate) rank: usize,
    /// The previous member, `p₁`; `None` for the holder.
    pub(crate) previous: Option<Did>,
    /// The next member.
    pub(crate) next: ChainNext,
}

/// The place of `local` in the chain of `key` under `segment`, or `None` when it is no member
/// (Lemma 2: its segment places its `replicas` previous holders, so it counts the holders from
/// the key's owner up to itself).
///
/// Post: `rank < replicas`; `previous` is the member before `local` iff `rank > 0`; `next` is
/// `End` iff `rank = replicas − 1` or no holder follows on a ring the view knows whole.
pub(crate) fn chain_position(
    segment: &RelaySegment,
    key: Did,
    local: Did,
    replicas: usize,
) -> Option<ChainPosition> {
    let RelayMembers::Members(members) = relay_members(segment, key, replicas) else {
        return None;
    };
    let rank = members.iter().position(|member| *member == local)?;
    let previous = rank
        .checked_sub(1)
        .and_then(|before| members.get(before))
        .copied();
    let next = if rank + 1 == replicas {
        ChainNext::End
    } else if let Some(next) = members.get(rank + 1) {
        ChainNext::Member(*next)
    } else if segment.knows_every_holder {
        ChainNext::End
    } else {
        ChainNext::BeyondView
    };
    Some(ChainPosition {
        rank,
        previous,
        next,
    })
}
