//! Named state predicates of the pure Chord topology.
//!
//! Every predicate is a proposition over one [`TopologyState`] `s` with
//! `n = s.local`, written against the clockwise metric `d(a, b) = (b - a) mod 2^160`
//! ([`dist`]). They are the single statement of the topology invariants, so a
//! model checker, a churn simulator, and a unit test all quote the same law
//! instead of re-deriving it from raw comparisons:
//!
//! - `SuccessorsWellFormed(s, k)`: the successor sequence is a strictly
//!   `d(n, ·)`-increasing chain of at most `k` remote peers.
//! - `PredecessorWellFormed(s)`: `pred ≠ n`.
//! - `FingersWellFormed(s)`: slot `i` holds no peer nearer than `2^i`, and
//!   occupied slots are `d(n, ·)`-monotone in the slot index.
//! - `RoutesClockwise(s, id)`: a remote hop toward `id` lies on the open arc
//!   `(n, id)`.
//! - `ChordFixpoint(s, M, k)`: `s` equals the image of the specification
//!   operators [`successors`], [`predecessor`], and [`finger`] on the member
//!   set `M`.
//!
//! The first four are safety invariants of [`step`](super::step): they hold in
//! every state reachable from a well-formed state. The last is the target of
//! stabilization, not an invariant.

use num_bigint::BigUint;

use super::dist;
use super::find_successor;
use super::finger;
use super::precedes;
use super::predecessor;
use super::successors;
use super::Did;
use super::FindSuccessorStep;
use super::TopologyState;

impl TopologyState {
    /// `SuccessorsWellFormed(s, k)`: `|succ| ≤ k ∧ ∀i. 0 < d(n, succ[i]) ∧
    /// ∀i < j. d(n, succ[i]) < d(n, succ[j])`.
    ///
    /// Strict monotonicity of the clockwise distance subsumes the three
    /// pointwise laws: the sequence is clockwise ordered, its entries are
    /// distinct, and none is `n` (whose distance is `0`).
    pub fn successors_are_well_formed(&self, capacity: usize) -> bool {
        let distances = self
            .successors
            .iter()
            .map(|successor| dist(self.local, *successor))
            .collect::<Vec<_>>();
        self.successors.len() <= capacity
            && distances
                .first()
                .is_none_or(|nearest| *nearest > BigUint::ZERO)
            && distances
                .iter()
                .zip(distances.iter().skip(1))
                .all(|(nearer, farther)| nearer < farther)
    }

    /// `PredecessorWellFormed(s)`: `pred ≠ n`, so the responsibility interval
    /// `(pred, n]` is never emptied by a self reference.
    pub fn predecessor_is_well_formed(&self) -> bool {
        self.predecessor != Some(self.local)
    }

    /// `FingersWellFormed(s)`: `∀i. finger[i] = f ⇒ d(n, f) ≥ 2^i`, and
    /// `∀i < j. finger[i] = f ∧ finger[j] = g ⇒ d(n, f) ≤ d(n, g)`.
    ///
    /// The first clause is the sparse/no-wrap law (it also excludes `n`, at
    /// distance `0 < 2^i`); the second says a higher slot never points nearer
    /// than a lower one, which is what lets `closest_preceding_finger` scan
    /// from the top.
    pub fn fingers_are_well_formed(&self) -> bool {
        let occupied = self
            .fingers
            .iter()
            .enumerate()
            .filter_map(|(slot, hint)| hint.map(|peer| (slot, dist(self.local, peer))))
            .collect::<Vec<_>>();
        occupied
            .iter()
            .all(|(slot, distance)| *distance >= BigUint::from(1u8) << *slot)
            && occupied
                .iter()
                .zip(occupied.iter().skip(1))
                .all(|((_, lower), (_, higher))| lower <= higher)
    }

    /// `RoutesClockwise(s, id)`: `find_successor(s, id) = Remote(next) ⇒
    /// next ∈ (n, id)`.
    ///
    /// A local answer satisfies the proposition vacuously; a remote hop must
    /// make strict clockwise progress, which is what bounds a routed lookup by
    /// the number of members.
    pub fn routes_clockwise_toward(&self, target: Did) -> bool {
        match find_successor(self, target) {
            FindSuccessorStep::Local(_) => true,
            FindSuccessorStep::Remote { next, .. } => {
                precedes(self.local, next, &dist(self.local, target))
            }
        }
    }

    /// `ChordFixpoint(s, M, k)`: `succ = Successors(M, n, k) ∧ pred =
    /// Predecessor(M, n) ∧ ∀i. finger[i] = Finger(M, n, i)`.
    ///
    /// `M` is the member set the overlay is judged against (the live nodes);
    /// the finger clause ranges over the slots this state actually carries.
    pub fn is_chord_fixpoint_of(&self, members: &[Did], capacity: usize) -> bool {
        self.successors == successors(members, self.local, capacity)
            && self.predecessor == predecessor(members, self.local)
            && self
                .fingers
                .iter()
                .enumerate()
                .all(|(slot, hint)| *hint == finger(members, self.local, slot))
    }
}

/// Each predicate is falsifiable: one witness per clause it states.
#[cfg(test)]
mod tests {
    use super::Did;
    use super::TopologyState;

    /// A state at identity `0` with the given successor, predecessor, and
    /// finger entries, written as small ring positions.
    fn state(
        successors: &[u32],
        predecessor: Option<u32>,
        fingers: &[Option<u32>],
    ) -> TopologyState {
        TopologyState::new(
            Did::from(0u32),
            successors.iter().copied().map(Did::from).collect(),
            predecessor.map(Did::from),
            fingers.iter().map(|hint| hint.map(Did::from)).collect(),
        )
    }

    /// Law: `SuccessorsWellFormed` rejects exactly an unordered, repeated,
    /// self-referencing, or over-capacity sequence.
    #[test]
    fn test_successor_well_formedness_rejects_each_violated_clause() {
        let cases = [
            (vec![], true),
            (vec![1, 2, 5], true),
            (vec![2, 1], false),
            (vec![1, 1], false),
            (vec![0, 1], false),
            (vec![1, 2, 3, 4], false),
        ];
        for (successors, expected) in cases {
            assert_eq!(
                state(&successors, None, &[]).successors_are_well_formed(3),
                expected,
                "{successors:?}"
            );
        }
    }

    /// Law: `PredecessorWellFormed` rejects exactly the self reference.
    #[test]
    fn test_predecessor_well_formedness_rejects_a_self_reference() {
        assert!(state(&[], None, &[]).predecessor_is_well_formed());
        assert!(state(&[], Some(7), &[]).predecessor_is_well_formed());
        assert!(!state(&[], Some(0), &[]).predecessor_is_well_formed());
    }

    /// Law: `FingersWellFormed` rejects a hint nearer than its slot's
    /// threshold and a higher slot that points nearer than a lower one.
    #[test]
    fn test_finger_well_formedness_rejects_each_violated_clause() {
        let cases = [
            (vec![None, None, None], true),
            (vec![Some(1), Some(2), Some(4)], true),
            (vec![Some(5), None, Some(5)], true),
            (vec![Some(0), None, None], false),
            (vec![None, Some(1), None], false),
            (vec![Some(6), None, Some(4)], false),
        ];
        for (fingers, expected) in cases {
            assert_eq!(
                state(&[], None, &fingers).fingers_are_well_formed(),
                expected,
                "{fingers:?}"
            );
        }
    }

    /// Law: `RoutesClockwise` holds even for representable ill-formed states,
    /// because `find_successor` skips self entries and hints beyond the target.
    #[test]
    fn test_routing_advances_clockwise_from_ill_formed_states() {
        let ill_formed = state(&[0, 3], Some(0), &[Some(0), Some(9), Some(3)]);
        for target in 0..12u32 {
            assert!(ill_formed.routes_clockwise_toward(Did::from(target)));
        }
    }

    /// Law: `ChordFixpoint` accepts the image of the specification operators
    /// and rejects a state that differs in any one component.
    #[test]
    fn test_chord_fixpoint_rejects_a_difference_in_any_component() {
        let members = [0u32, 1, 2, 4].map(Did::from);
        let fixpoint = state(&[1, 2], Some(4), &[Some(1), Some(2), Some(4)]);
        assert!(fixpoint.is_chord_fixpoint_of(&members, 2));
        for differing in [
            state(&[1], Some(4), &[Some(1), Some(2), Some(4)]),
            state(&[1, 2], Some(2), &[Some(1), Some(2), Some(4)]),
            state(&[1, 2], Some(4), &[Some(1), Some(4), Some(4)]),
        ] {
            assert!(!differing.is_chord_fixpoint_of(&members, 2));
        }
    }
}
