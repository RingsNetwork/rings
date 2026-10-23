//! Table and model tests of the relay holder law.
//!
//! The table tests pin `relay_holder` on explicit segments. The model tests build a whole ring
//! with its declarations (the truth), give every node the view it has in production (its
//! predecessor, `r` successors, the declarations of its neighbours, and the previous holder its
//! predecessor passes it, run to a fixpoint as repeated repair passes would), and judge every
//! view against the truth: what a view names is the truth, a holder places every key it holds,
//! a writer's choice is admitted where it sends the hold, and on a membership change the old
//! holder offers each carrier to the new one, which takes it.

use super::in_arc;
use super::relay_handoff_target;
use super::relay_holder;
use super::HoldAuthority;
use super::RelayCustody;
use super::RelayHolder;
use super::RelayNeighborhood;
use super::RelayNode;
use super::RelaySegment;
use crate::dht::topology::TopologyState;
use crate::dht::Did;

/// The node at ring position `position`.
fn did(position: u32) -> Did {
    Did::from(position)
}

/// `did(position)`, declaring `holds`.
fn node(position: u32, holds: bool) -> RelayNode {
    RelayNode {
        did: did(position),
        holds,
    }
}

#[test]
fn test_holder_is_the_owner_when_it_holds_relay_mail() {
    let segment = RelaySegment::new(vec![node(10, true), node(20, true), node(30, true)]);
    assert_eq!(
        relay_holder(&segment, did(15)),
        RelayHolder::Holder(did(10))
    );
    assert_eq!(
        relay_holder(&segment, did(20)),
        RelayHolder::Holder(did(10))
    );
    assert_eq!(
        relay_holder(&segment, did(21)),
        RelayHolder::Holder(did(20))
    );
}

#[test]
fn test_holder_skips_an_owner_and_its_next_successors_that_hold_none() {
    // The owner of 11 is 10; it and its next three successors hold no relay mail.
    let segment = RelaySegment::new(vec![
        node(10, false),
        node(20, false),
        node(30, false),
        node(40, false),
        node(50, true),
        node(60, true),
    ]);
    assert_eq!(
        relay_holder(&segment, did(11)),
        RelayHolder::Holder(did(50))
    );
    assert_eq!(
        relay_holder(&segment, did(45)),
        RelayHolder::Holder(did(50))
    );
    assert_eq!(
        relay_holder(&segment, did(55)),
        RelayHolder::Holder(did(50))
    );
}

#[test]
fn test_no_holder_up_to_the_end_of_the_view_is_unheld() {
    let segment = RelaySegment::new(vec![node(10, true), node(20, false), node(30, false)]);
    assert_eq!(relay_holder(&segment, did(25)), RelayHolder::Unheld);
    assert_eq!(
        relay_holder(&segment, did(15)),
        RelayHolder::Holder(did(10))
    );
}

#[test]
fn test_a_key_whose_owner_the_view_does_not_place_is_unknown() {
    let segment = RelaySegment::new(vec![node(10, true), node(20, true)]);
    // Past the last node, and behind the first: the owner is outside the view.
    assert_eq!(relay_holder(&segment, did(25)), RelayHolder::Unknown);
    assert_eq!(relay_holder(&segment, did(5)), RelayHolder::Unknown);
    assert_eq!(relay_holder(&segment, did(10)), RelayHolder::Unknown);
}

#[test]
fn test_a_closing_segment_owns_the_rest_of_the_ring_and_wraps_its_search() {
    let alone = RelaySegment::closing(vec![node(10, true)]);
    assert_eq!(relay_holder(&alone, did(3)), RelayHolder::Holder(did(10)));
    assert_eq!(relay_holder(&alone, did(10)), RelayHolder::Holder(did(10)));
    let alone_without_store = RelaySegment::closing(vec![node(10, false)]);
    assert_eq!(
        relay_holder(&alone_without_store, did(3)),
        RelayHolder::Unheld
    );
    // A node with a predecessor and no successor owns the arc back to the predecessor, and the
    // search wraps round to the predecessor.
    let closing = RelaySegment::closing(vec![node(10, true), node(20, false)]);
    assert_eq!(
        relay_holder(&closing, did(25)),
        RelayHolder::Holder(did(10))
    );
    assert_eq!(relay_holder(&closing, did(5)), RelayHolder::Holder(did(10)));
    assert_eq!(
        relay_holder(&closing, did(15)),
        RelayHolder::Holder(did(10))
    );
}

#[test]
fn test_hand_off_target_is_the_holder_or_the_last_holding_node_of_the_view() {
    let segment = RelaySegment::new(vec![node(10, true), node(20, false), node(30, true)]);
    assert_eq!(
        relay_handoff_target(&segment, did(15), did(30)),
        Some(did(10))
    );
    assert_eq!(
        relay_handoff_target(&segment, did(25), did(10)),
        Some(did(30))
    );
    // This node holds it: nothing to offer.
    assert_eq!(relay_handoff_target(&segment, did(15), did(10)), None);
    // Past the view: offered to the last node, which holds relay mail.
    assert_eq!(
        relay_handoff_target(&segment, did(35), did(10)),
        Some(did(30))
    );
    let ending_without_store = RelaySegment::new(vec![node(10, true), node(20, false)]);
    assert_eq!(
        relay_handoff_target(&ending_without_store, did(25), did(10)),
        None
    );
}

#[test]
fn test_hold_authority_admits_the_responsible_node_or_a_storage_less_node_of_the_run() {
    let holds = |peer: Did| peer != did(20) && peer != did(30);
    assert!(HoldAuthority::Node(did(30)).admits(did(30), holds));
    assert!(!HoldAuthority::Node(did(30)).admits(did(20), holds));
    let run = HoldAuthority::Run {
        first: did(18),
        last: did(30),
    };
    assert!(run.admits(did(20), holds));
    assert!(run.admits(did(30), holds));
    // Behind the destination, or holding relay mail, or past the run: not admitted.
    assert!(!run.admits(did(15), |_| false));
    assert!(!run.admits(did(25), |_| true));
    assert!(!run.admits(did(35), |_| false));
    assert!(!HoldAuthority::Unknown.admits(did(20), holds));
}

/// A whole ring: every node, clockwise from position 0, with its declaration.
#[derive(Clone, Debug)]
struct Ring {
    /// The nodes, sorted by position.
    nodes: Vec<RelayNode>,
    /// The successor list capacity `r` of every node.
    successors: usize,
}

impl Ring {
    /// The ring of `nodes` (any order) with successor lists of `successors`.
    fn new(mut nodes: Vec<RelayNode>, successors: usize) -> Self {
        nodes.sort_by_key(|node| node.did);
        Self { nodes, successors }
    }

    /// This ring with `node` joined.
    fn joined(&self, node: RelayNode) -> Self {
        let mut nodes = self.nodes.clone();
        nodes.push(node);
        Self::new(nodes, self.successors)
    }

    /// This ring without the node at `position`.
    fn left(&self, position: u32) -> Self {
        let nodes = self
            .nodes
            .iter()
            .copied()
            .filter(|node| node.did != did(position))
            .collect();
        Self::new(nodes, self.successors)
    }

    /// The index of `node`.
    fn index(&self, node: Did) -> usize {
        self.nodes
            .iter()
            .position(|known| known.did == node)
            .unwrap_or_else(|| panic!("{node} is not on the ring"))
    }

    /// The node `steps` after the node at index `i`.
    fn after(&self, i: usize, steps: usize) -> RelayNode {
        self.nodes[(i + steps) % self.nodes.len()]
    }

    /// The index of the owner of `key`: the node `n` with `key ∈ (n, succ(n)]`.
    fn owner(&self, key: Did) -> usize {
        (0..self.nodes.len())
            .find(|i| {
                self.nodes.len() == 1 || in_arc(self.nodes[*i].did, key, self.after(*i, 1).did)
            })
            .unwrap_or_else(|| panic!("no owner of {key}"))
    }

    /// The truth: the first node from the owner of `key` round the ring that holds relay mail.
    fn holder(&self, key: Did) -> Option<Did> {
        let owner = self.owner(key);
        (0..self.nodes.len())
            .map(|step| self.after(owner, step))
            .find(|node| node.holds)
            .map(|node| node.did)
    }

    /// The node responsible for the position `destination`: the first node at or after it.
    fn responsible(&self, destination: Did) -> Did {
        let owner = self.owner(destination);
        if self.nodes[owner].did == destination {
            destination
        } else {
            self.after(owner, 1).did
        }
    }

    /// The topology state of the node at index `i`.
    fn state(&self, i: usize) -> TopologyState {
        let count = self.successors.min(self.nodes.len() - 1);
        let successors = (1..=count).map(|step| self.after(i, step).did).collect();
        let predecessor = (self.nodes.len() > 1).then(|| self.after(i, self.nodes.len() - 1).did);
        TopologyState::new(self.nodes[i].did, successors, predecessor, Vec::new())
    }

    /// Every node's neighbourhood once repair passes reach a fixpoint: the declarations of its
    /// neighbours, and the previous holder its predecessor passes it.
    fn neighborhoods(&self) -> Vec<RelayNeighborhood> {
        let mut neighborhoods = (0..self.nodes.len())
            .map(|i| {
                let mut neighborhood = RelayNeighborhood::default();
                let state = self.state(i);
                for peer in state.successors.iter().chain(state.predecessor.iter()) {
                    let declared = self.nodes[self.index(*peer)].holds;
                    neighborhood.observe(*peer, RelayCustody::of(declared));
                }
                neighborhood
            })
            .collect::<Vec<_>>();
        // A record crosses one link per pass, so as many passes as nodes reach the fixpoint.
        for _ in 0..=self.nodes.len() {
            for i in 0..self.nodes.len() {
                let passed = neighborhoods[i].record_to_pass(&self.state(i), self.nodes[i].holds);
                if let Some((head, previous)) = passed {
                    let receiver = self.index(head);
                    let from = self.nodes[i].did;
                    neighborhoods[receiver].pass(&self.state(receiver), from, previous);
                }
            }
        }
        neighborhoods
    }

    /// The segment of the node at index `i` under `neighborhoods`.
    fn segment(&self, neighborhoods: &[RelayNeighborhood], i: usize) -> RelaySegment {
        neighborhoods[i].segment(&self.state(i), self.nodes[i].holds)
    }

    /// The keys worth judging: every arc's first and middle position.
    fn keys(&self) -> Vec<Did> {
        (0..self.nodes.len())
            .flat_map(|i| {
                let start = self.nodes[i].did;
                [start + did(1), start + did(3)]
            })
            .collect()
    }
}

/// The ring of `positions` with `storage_less` declaring no relay store.
fn ring(positions: &[u32], storage_less: &[u32], successors: usize) -> Ring {
    let nodes = positions
        .iter()
        .map(|position| node(*position, !storage_less.contains(position)))
        .collect();
    Ring::new(nodes, successors)
}

/// Judge every view of `ring` against its truth (see the module documentation).
fn assert_views_agree_with_the_truth(ring: &Ring) {
    let neighborhoods = ring.neighborhoods();
    for key in ring.keys() {
        let truth = ring.holder(key);
        for i in 0..ring.nodes.len() {
            let local = ring.nodes[i].did;
            match relay_holder(&ring.segment(&neighborhoods, i), key) {
                // Soundness: a view names the true holder, or none.
                RelayHolder::Holder(holder) => {
                    assert_eq!(Some(holder), truth, "{local} names {holder} for {key}");
                }
                RelayHolder::Unheld | RelayHolder::Unknown => {}
            }
        }
        let Some(holder) = truth else {
            continue;
        };
        // Coverage: the holder places every key it holds, so it takes what it is sent.
        let h = ring.index(holder);
        assert_eq!(
            relay_holder(&ring.segment(&neighborhoods, h), key),
            RelayHolder::Holder(holder),
            "the holder {holder} does not place {key}"
        );
    }
}

/// The writer of a hold for an offline `destination` (the node responsible for it) names the
/// true holder of its inbox key, and the holder admits that writer.
fn assert_hold_reaches_the_holder(ring: &Ring, destination: Did) {
    let neighborhoods = ring.neighborhoods();
    let key = destination + did(1);
    let writer = ring.responsible(destination);
    let w = ring.index(writer);
    let truth = ring.holder(key).expect("the ring holds relay mail");
    assert_eq!(
        relay_holder(&ring.segment(&neighborhoods, w), key),
        RelayHolder::Holder(truth)
    );
    let h = ring.index(truth);
    let authority = neighborhoods[h].hold_authority(&ring.state(h), destination);
    assert!(
        authority.admits(writer, |peer| neighborhoods[h].holds(peer)),
        "{truth} does not admit {writer}'s hold for {destination}: {authority:?}"
    );
}

/// Every carrier whose holder moves from `before` to `after` is offered by the old holder to the
/// new one, which takes it from that sender; every carrier whose holder stays is not offered.
fn assert_carriers_follow_the_holder(before: &Ring, after: &Ring) {
    let neighborhoods = after.neighborhoods();
    for key in before.keys().into_iter().chain(after.keys()) {
        let (Some(old), Some(new)) = (before.holder(key), after.holder(key)) else {
            continue;
        };
        let Some(o) = after.nodes.iter().position(|node| node.did == old) else {
            continue;
        };
        let offered = relay_handoff_target(&after.segment(&neighborhoods, o), key, old);
        if old == new {
            assert_eq!(offered, None, "{old} offers {key}, which it still holds");
            continue;
        }
        assert_eq!(
            offered,
            Some(new),
            "{old} offers {key} elsewhere than {new}"
        );
        let n = after.index(new);
        assert!(neighborhoods[n].relocates_from(&after.state(n), old));
    }
}

#[test]
fn test_model_views_agree_with_the_truth_on_mixed_rings() {
    let positions = [100, 200, 300, 400, 500, 600, 700, 800];
    for storage_less in [
        &[][..],
        &[200],
        &[200, 300],
        &[200, 300, 400, 500],
        &[100, 300, 500, 700],
        &[100, 200, 300, 400, 500, 600, 700],
        &[100, 200, 300, 400, 500, 600, 700, 800],
    ] {
        for successors in [1, 2, 3, 8] {
            assert_views_agree_with_the_truth(&ring(&positions, storage_less, successors));
        }
    }
}

#[test]
fn test_model_owner_and_its_next_successors_hold_none() {
    // The owner of 101 is 100; it and its next three successors hold no relay mail, and the
    // fourth successor 500 is the holder, within the owner's successor list of four.
    let ring = ring(&[100, 200, 300, 400, 500, 600], &[100, 200, 300, 400], 4);
    assert_eq!(ring.holder(did(101)), Some(did(500)));
    assert_views_agree_with_the_truth(&ring);
    let neighborhoods = ring.neighborhoods();
    for owner_side in [100, 200, 300, 400] {
        let i = ring.index(did(owner_side));
        assert_eq!(
            relay_holder(&ring.segment(&neighborhoods, i), did(101)),
            RelayHolder::Holder(did(500))
        );
    }
    // Every destination whose inbox key lies on the run is held at 500 and admitted there.
    for destination in [150, 250, 350, 450] {
        assert_hold_reaches_the_holder(&ring, did(destination));
    }
    // The recipient 300, without a relay store, fetches its inbox from 500.
    let recipient = ring.index(did(300));
    assert_eq!(
        relay_holder(&ring.segment(&neighborhoods, recipient), did(301)),
        RelayHolder::Holder(did(500))
    );
}

#[test]
fn test_model_a_holder_past_the_view_is_unheld_and_never_misnamed() {
    // With two successors, the owner 100 sees 200 and 300, which hold none either: its view
    // names no holder, so it refuses a hold rather than guess.
    let ring = ring(&[100, 200, 300, 400, 500], &[100, 200, 300], 2);
    let neighborhoods = ring.neighborhoods();
    let owner = ring.index(did(100));
    assert_eq!(
        relay_holder(&ring.segment(&neighborhoods, owner), did(101)),
        RelayHolder::Unheld
    );
    assert_views_agree_with_the_truth(&ring);
}

#[test]
fn test_model_a_lone_holder_places_every_key_of_the_ring() {
    let ring = ring(&[100, 200, 300, 400, 500], &[100, 200, 300, 400], 2);
    assert_views_agree_with_the_truth(&ring);
    assert_hold_reaches_the_holder(&ring, did(350));
    assert_hold_reaches_the_holder(&ring, did(450));
    // The writer 200 of a hold for 150 sees no holder within its two successors, and refuses.
    let neighborhoods = ring.neighborhoods();
    let writer = ring.index(ring.responsible(did(150)));
    assert_eq!(
        relay_holder(&ring.segment(&neighborhoods, writer), did(151)),
        RelayHolder::Unheld
    );
}

#[test]
fn test_model_a_storage_less_node_joining_between_the_key_and_the_holder_moves_nothing() {
    let before = ring(&[100, 200, 400, 600], &[100, 200], 3);
    // 300 joins between the key 101 and its holder 400, holding no relay mail.
    let after = before.joined(node(300, false));
    assert_eq!(before.holder(did(101)), Some(did(400)));
    assert_eq!(after.holder(did(101)), Some(did(400)));
    assert_views_agree_with_the_truth(&after);
    assert_carriers_follow_the_holder(&before, &after);
    // It leaves again: still nothing moves.
    assert_carriers_follow_the_holder(&after, &before);
}

#[test]
fn test_model_a_storage_less_node_joining_as_the_owner_moves_custody_forward() {
    // 150 joins and owns the key 151, held by 100 until now; custody moves to 200.
    let before = ring(&[100, 200, 300], &[], 2);
    let after = before.joined(node(150, false));
    assert_eq!(before.holder(did(151)), Some(did(100)));
    assert_eq!(after.holder(did(151)), Some(did(200)));
    assert_views_agree_with_the_truth(&after);
    assert_carriers_follow_the_holder(&before, &after);
}

#[test]
fn test_model_a_holder_joining_inside_a_run_takes_custody_back() {
    // 250 joins inside the storage-less run 100..400 and holds relay mail: it becomes the holder
    // of the keys owned on the run behind it, which 500 held until now.
    let before = ring(&[100, 200, 300, 400, 500, 600], &[100, 200, 300, 400], 4);
    let after = before.joined(node(250, true));
    assert_eq!(before.holder(did(101)), Some(did(500)));
    assert_eq!(after.holder(did(101)), Some(did(250)));
    assert_eq!(after.holder(did(301)), Some(did(500)));
    assert_views_agree_with_the_truth(&after);
    assert_carriers_follow_the_holder(&before, &after);
    // It leaves again: custody moves forward to 500.
    assert_carriers_follow_the_holder(&after, &before.left(250));
}
