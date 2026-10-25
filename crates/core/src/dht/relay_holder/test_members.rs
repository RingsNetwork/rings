//! Table and model tests of replicated custody (#777; `Law (Members)`, `Law (Previous
//! holders)`, `Law (Chain repair)`).
//!
//! The table tests pin `relay_members` and `relay_carrier_offer` on explicit segments. The model
//! tests build whole rings with the views of `tests` (each node's predecessor, successor list,
//! neighbours' declarations, and the previous holders its predecessor passes it, run to a
//! fixpoint) and judge every view against the truth `M_r(k)`, the first `r` nodes from the owner
//! of `k` round the ring that hold relay mail: what a view names is a prefix of the truth, every
//! member counts itself one, every two members admit each other's copies, and after a holder
//! leaves every surviving copy reaches the new members.

use super::chain_position;
use super::relay_carrier_offer;
use super::relay_holder;
use super::relay_members;
use super::tests::did;
use super::tests::node;
use super::tests::ring;
use super::tests::Ring;
use super::ChainNext;
use super::ChainPosition;
use super::RelayHolder;
use super::RelayMembers;
use super::RelaySegment;
use crate::consts::DEFAULT_RELAY_INBOX_REPLICAS;
use crate::dht::Did;
use crate::dht::StorageSyncPurpose;

/// `RelayMembers::Members` of the nodes at `positions`.
fn members(positions: &[u32]) -> RelayMembers {
    RelayMembers::Members(positions.iter().map(|position| did(*position)).collect())
}

#[test]
fn test_members_are_the_first_holders_from_the_owner() {
    let segment = RelaySegment::new(vec![
        node(10, true),
        node(20, false),
        node(30, true),
        node(40, true),
        node(50, true),
        node(60, true),
    ]);
    assert_eq!(relay_members(&segment, did(15), 3), members(&[10, 30, 40]));
    // The owner of 25 holds no relay mail, so the members start past it.
    assert_eq!(relay_members(&segment, did(25), 3), members(&[30, 40, 50]));
    assert_eq!(relay_members(&segment, did(15), 1), members(&[10]));
}

#[test]
fn test_members_stop_where_the_view_ends() {
    let segment = RelaySegment::new(vec![node(10, true), node(20, false), node(30, true)]);
    assert_eq!(relay_members(&segment, did(15), 3), members(&[10, 30]));
    assert_eq!(relay_members(&segment, did(25), 3), members(&[30]));
    assert_eq!(relay_members(&segment, did(5), 3), RelayMembers::Unknown);
    let storage_less = RelaySegment::new(vec![node(10, false), node(20, false)]);
    assert_eq!(
        relay_members(&storage_less, did(15), 3),
        RelayMembers::Unheld
    );
}

#[test]
fn test_the_first_member_is_the_holder() {
    let segments = [
        RelaySegment::new(vec![node(10, false), node(20, true), node(30, true)]),
        RelaySegment::new(vec![node(10, false), node(20, false)]),
        RelaySegment::closing(vec![node(10, true), node(20, false)]),
    ];
    for segment in &segments {
        for key in [5, 15, 25, 35].map(did) {
            let first = match relay_members(segment, key, DEFAULT_RELAY_INBOX_REPLICAS) {
                RelayMembers::Members(members) => RelayHolder::Holder(members[0]),
                RelayMembers::Unheld => RelayHolder::Unheld,
                RelayMembers::Unknown => RelayHolder::Unknown,
            };
            assert_eq!(first, relay_holder(segment, key), "{segment:?} at {key}");
        }
    }
}

#[test]
fn test_a_member_copies_and_a_non_member_hands_off() {
    let segment = RelaySegment::new(vec![
        node(10, true),
        node(20, true),
        node(30, true),
        node(40, true),
    ]);
    let copy = relay_carrier_offer(&segment, did(15), did(20), None, 3);
    assert_eq!(copy.purpose, StorageSyncPurpose::AdditiveRepair);
    assert_eq!(copy.targets, [did(10), did(30)]);
    let handoff = relay_carrier_offer(&segment, did(15), did(40), None, 3);
    assert_eq!(handoff.purpose, StorageSyncPurpose::OwnershipHandoff);
    assert_eq!(handoff.targets, [did(10), did(20), did(30)]);
    // A key the view does not place goes to both ends that hold relay mail.
    let unknown = relay_carrier_offer(&segment, did(5), did(20), None, 3);
    assert_eq!(unknown.purpose, StorageSyncPurpose::OwnershipHandoff);
    assert_eq!(unknown.targets, [did(40), did(10)]);
}

/// The truth `M_r(k)`: the first `DEFAULT_RELAY_INBOX_REPLICAS` distinct nodes from the owner of `key`
/// round the ring that hold relay mail.
fn true_members(ring: &Ring, key: Did) -> Vec<Did> {
    let owner = ring.owner(key);
    let mut members = Vec::new();
    for step in 0..ring.nodes.len() {
        let candidate = ring.after(owner, step);
        if candidate.holds && members.len() < DEFAULT_RELAY_INBOX_REPLICAS {
            members.push(candidate.did);
        }
    }
    members
}

/// The members the view of the node at index `i` names for `key`, empty for none.
fn named(ring: &Ring, i: usize, key: Did) -> Vec<Did> {
    let neighborhoods = ring.neighborhoods();
    match relay_members(
        &ring.segment(&neighborhoods, i),
        key,
        DEFAULT_RELAY_INBOX_REPLICAS,
    ) {
        RelayMembers::Members(members) => members,
        RelayMembers::Unheld | RelayMembers::Unknown => Vec::new(),
    }
}

/// Judge every view of `ring` against the truth (see the module documentation).
fn assert_members_agree_with_the_truth(ring: &Ring) {
    for key in ring.keys() {
        let truth = true_members(ring, key);
        for i in 0..ring.nodes.len() {
            // Soundness: a view names a prefix of the truth.
            let named = named(ring, i, key);
            assert_eq!(
                named,
                truth[..named.len()],
                "{} names {named:?} for {key}, the truth is {truth:?}",
                ring.nodes[i].did
            );
        }
        for member in &truth {
            // Coverage: every member names every member, itself included, so it persists a
            // copy and admits every other member's (`Law (Replica admission)`).
            let named = named(ring, ring.index(*member), key);
            assert_eq!(named, truth, "member {member} names {named:?} for {key}");
        }
    }
}

#[test]
fn test_model_members_agree_with_the_truth() {
    let positions = [10, 20, 30, 40, 50, 60, 70, 80];
    assert_members_agree_with_the_truth(&ring(&positions, &[], 3));
    assert_members_agree_with_the_truth(&ring(&positions, &[20, 50], 4));
    // Runs of two storage-less nodes: a successor list of r + L covers the members.
    assert_members_agree_with_the_truth(&ring(&positions, &[20, 30, 60, 70], 5));
}

#[test]
fn test_model_a_ring_of_fewer_holders_than_replicas_names_them_all() {
    assert_members_agree_with_the_truth(&ring(&[10, 20, 30], &[20], 2));
    assert_members_agree_with_the_truth(&ring(&[10, 20], &[], 1));
}

/// Every copy that survives the change from `before` to `after` reaches the new members: a
/// keeper that is still a member copies the carrier to every other new member, each of which
/// admits it; a keeper that is no member any more hands it to new members, each of which takes
/// it as a member (`Law (Chain repair)`).
fn assert_copies_reach_the_new_members(before: &Ring, after: &Ring) {
    let neighborhoods = after.neighborhoods();
    for key in after.keys() {
        let truth = true_members(after, key);
        for keeper in true_members(before, key) {
            if !after.nodes.iter().any(|known| known.did == keeper) {
                continue;
            }
            let k = after.index(keeper);
            let offer = relay_carrier_offer(
                &after.segment(&neighborhoods, k),
                key,
                keeper,
                None,
                DEFAULT_RELAY_INBOX_REPLICAS,
            );
            if truth.contains(&keeper) {
                assert_eq!(offer.purpose, StorageSyncPurpose::AdditiveRepair);
                for member in truth.iter().filter(|member| **member != keeper) {
                    assert!(
                        offer.targets.contains(member),
                        "{keeper} does not copy {key} to {member}: {offer:?}"
                    );
                    let named = named(after, after.index(*member), key);
                    assert!(
                        named.contains(member) && named.contains(&keeper),
                        "{member} does not admit {keeper}'s copy of {key}"
                    );
                }
            } else {
                assert_eq!(offer.purpose, StorageSyncPurpose::OwnershipHandoff);
                assert!(
                    !offer.targets.is_empty(),
                    "{keeper} keeps {key} offered nowhere"
                );
                for target in &offer.targets {
                    assert!(
                        named(after, after.index(*target), key).contains(target),
                        "{target} does not take {keeper}'s hand-off of {key}"
                    );
                }
            }
        }
    }
}

#[test]
fn test_model_copies_survive_the_loss_of_a_holder() {
    let positions = [10, 20, 30, 40, 50, 60, 70, 80];
    let before = ring(&positions, &[50], 4);
    for leaving in [10, 30, 60] {
        assert_copies_reach_the_new_members(&before, &before.left(leaving));
    }
}

#[test]
fn test_model_copies_reach_a_joining_holder() {
    let before = ring(&[10, 30, 50, 70], &[], 3);
    let after = before.joined(node(40, true));
    assert_copies_reach_the_new_members(&before, &after);
}

#[test]
fn test_a_chain_position_names_the_previous_and_next_member() {
    let segment = RelaySegment::new(vec![
        node(10, true),
        node(20, true),
        node(30, false),
        node(40, true),
        node(50, true),
    ]);
    // Key 15: owner 10; the members from it are 10, 20, 40, 50.
    assert_eq!(
        chain_position(&segment, did(15), did(10), 8),
        Some(ChainPosition {
            rank: 0,
            previous: None,
            next: ChainNext::Member(did(20)),
        })
    );
    assert_eq!(
        chain_position(&segment, did(15), did(20), 8),
        Some(ChainPosition {
            rank: 1,
            previous: Some(did(10)),
            next: ChainNext::Member(did(40)),
        })
    );
    // The last member the view places, before rank r − 1: its next member lies past the view.
    assert_eq!(
        chain_position(&segment, did(15), did(50), 8).map(|position| position.next),
        Some(ChainNext::BeyondView)
    );
    // At rank r − 1 the chain ends.
    assert_eq!(
        chain_position(&segment, did(15), did(40), 3).map(|position| position.next),
        Some(ChainNext::End)
    );
    assert_eq!(chain_position(&segment, did(15), did(50), 3), None);
}

#[test]
fn test_a_chain_ends_early_on_a_ring_the_view_knows_whole() {
    // Three holders, r = 8: a view that knows every holder ends the chain at the last of them.
    let segment = RelaySegment::new(vec![node(10, true), node(20, true), node(30, true)])
        .knowing_every_holder();
    assert_eq!(
        chain_position(&segment, did(15), did(30), 8).map(|position| position.next),
        Some(ChainNext::End)
    );
    let open = RelaySegment::new(vec![node(10, true), node(20, true), node(30, true)]);
    assert_eq!(
        chain_position(&open, did(15), did(30), 8).map(|position| position.next),
        Some(ChainNext::BeyondView)
    );
}

/// Every member of a key names its previous member as its own view does, and the chain built
/// from the next members it names, followed from the holder, is the truth `M_r(k)` up to where a
/// view ends: an `r = 8` chain on a small ring ends at its last holder.
fn assert_chains_agree_with_the_truth(ring: &Ring, replicas: usize) {
    let neighborhoods = ring.neighborhoods();
    for key in ring.keys() {
        let owner = ring.owner(key);
        let truth = (0..ring.nodes.len())
            .map(|step| ring.after(owner, step))
            .filter(|node| node.holds)
            .map(|node| node.did)
            .take(replicas)
            .collect::<Vec<_>>();
        for (rank, member) in truth.iter().enumerate() {
            let segment = ring.segment(&neighborhoods, ring.index(*member));
            let position = chain_position(&segment, key, *member, replicas)
                .unwrap_or_else(|| panic!("{member} is no member of {key}"));
            assert_eq!(position.rank, rank, "{member} at {key}");
            assert_eq!(
                position.previous,
                rank.checked_sub(1).map(|before| truth[before])
            );
            match position.next {
                ChainNext::Member(next) => assert_eq!(Some(&next), truth.get(rank + 1)),
                ChainNext::End => assert_eq!(rank + 1, truth.len(), "{member} ends {key} early"),
                ChainNext::BeyondView => assert!(rank + 1 < truth.len()),
            }
        }
    }
}

#[test]
fn test_model_chains_agree_with_the_truth_at_eight_replicas() {
    let positions = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    assert_chains_agree_with_the_truth(&ring(&positions, &[], 3), 8);
    assert_chains_agree_with_the_truth(&ring(&positions, &[30, 70], 3), 8);
}

#[test]
fn test_model_an_eight_replica_chain_on_a_small_ring_ends_at_its_last_holder() {
    assert_chains_agree_with_the_truth(&ring(&[10, 20, 30, 40], &[], 3), 8);
    assert_chains_agree_with_the_truth(&ring(&[10, 20, 30], &[20], 2), 8);
}
