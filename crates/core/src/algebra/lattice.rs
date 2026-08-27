#[cfg(test)]
use std::fmt::Debug;

/// Join-semilattice for state-based CRDT merge.
///
/// `join` returns the least upper bound of two states from the same carrier.
/// Implementors must make this operation inflationary with respect to the
/// carrier's induced partial order `a <= b iff a.join(b) == b`; because Rust
/// does not expose that order here, the obligation is witnessed through the
/// idempotence, commutativity, and associativity laws below.
///
/// The operation takes both arguments by value to model a pure state transition
/// into a canonical least upper bound. Implementations that need an in-place
/// merge can provide that adapter separately and keep this law-facing
/// signature as the common algebraic surface.
///
/// Law: `a.join(a) == a`.
///
/// Law: `a.join(b) == b.join(a)`.
///
/// Law: `a.join(b).join(c) == a.join(b.join(c))`.
pub trait JoinSemilattice: Sized {
    /// Return the least upper bound of `self` and `other`.
    fn join(self, other: Self) -> Self;
}

/// Assert join-semilattice laws for a representative finite sample.
///
/// This helper checks the state-based CRDT merge laws that imply strong
/// eventual consistency for replicas that observe the same set of deltas in any
/// order, with any duplication.
#[cfg(test)]
pub fn assert_join_semilattice_laws<T>(values: &[T])
where T: JoinSemilattice + Clone + Eq + Debug {
    for a in values {
        assert_eq!(a.clone().join(a.clone()), *a);

        for b in values {
            assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));

            for c in values {
                let lhs = a.clone().join(b.clone()).join(c.clone());
                let rhs = a.clone().join(b.clone().join(c.clone()));
                assert_eq!(lhs, rhs);
            }
        }
    }
}

/// Assert strong eventual consistency for one base state and a finite delta set.
///
/// The witness applies the same deltas in forward, reverse, and duplicated
/// schedules. A lawful join-semilattice must materialize the same least upper
/// bound for each schedule.
#[cfg(test)]
pub fn assert_strong_eventual_consistency<T>(base: T, deltas: &[T])
where T: JoinSemilattice + Clone + Eq + Debug {
    let forward = deltas
        .iter()
        .cloned()
        .fold(base.clone(), JoinSemilattice::join);
    let reverse = deltas
        .iter()
        .rev()
        .cloned()
        .fold(base.clone(), JoinSemilattice::join);
    let duplicated = deltas
        .iter()
        .cloned()
        .chain(deltas.iter().cloned())
        .fold(base, JoinSemilattice::join);

    assert_eq!(forward, reverse);
    assert_eq!(forward, duplicated);
}
