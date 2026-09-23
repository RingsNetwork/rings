//! Onion loops: the closed hop sequence a pipeline is evaluated on (#834 D4c, D5, L7).
//!
//! A pipeline `P = (f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)` is evaluated on the client-sealed loop
//!
//! ```text
//! LoopOf(P):  cl → r₀,₁ … r₀,ₛ → h₁ → r₁,₁ … r₁,ₛ → ⋯ → hₙ → rₙ,₁ … rₙ,ₛ → cl
//! ```
//!
//! of `n + 1` segments of `s = ONION_SEGMENT_RELAYS` relays, closed by the client's entry guard
//! `g = r₀,₁ = rₙ,ₛ`: the guard is relay `1` of the first segment and relay `s` of the last. The
//! positions are `i ∈ [1, H]`, and the role of each is a function of `i` alone:
//!
//! ```text
//! role(i) = Guard       i = 1 ∨ i = H
//!         = Symbol(k)   i = (s + 1)·k,  1 ≤ k ≤ n
//!         = Relay       otherwise
//! ```
//!
//! Laws, for every shape `n ∈ [1, n_max]`:
//!
//! - **Size.** `H(n, s) = (s + 1)·n + s` positions and `H − 1` distinct hops, bounded by
//!   `MAX_ONION_LOOP_HOPS = H(n_max, s) = 14` with `n_max = MAX_ONION_LOOP_SYMBOLS = 4`.
//! - **Segment** (D5). Exactly `s` relay positions, the guard counted, precede `h₁`, separate
//!   `hₖ` from `hₖ₊₁` and follow `hₙ`: a short segment is unrepresentable.
//! - **Guard closure** (L7). An [`OnionLoop`] stores the guard once and places it at positions `1`
//!   and `H` and nowhere else, so the guard is the only hop adjacent to the client. The remaining
//!   `H − 2` positions are pairwise distinct and distinct from `g` for every loop a route admits
//!   (`has_duplicate_dids` in the route module).
//!
//! Categorically, [`OnionLoopShape::try_label`] is the traversal of the shape's positions in the
//! failure monad: it labels the `H − 1` positions of the open path `1 … H − 1` in order and closes
//! the loop with the guard's label, so `try_label` never labels the guard twice.

use std::iter;

use super::OnionRouteError;
use crate::error::Error;
use crate::error::Result;

/// Number `s` of relays in every segment of a loop, the guard counted in both end segments (D5).
///
/// `s = 2` is the least value at which no single node is adjacent to both the client and a symbol
/// hop: at `s = 1` the guard alone would neighbour the client, `h₁` and `hₙ`.
pub const ONION_SEGMENT_RELAYS: usize = 2;

/// Largest number `n_max` of symbol applications in one pipeline (#834 D4a).
pub const MAX_ONION_LOOP_SYMBOLS: usize = 4;

/// Largest number of positions of one loop, `H(n_max, s) = (s + 1)·n_max + s = 14`, derived from
/// [`ONION_SEGMENT_RELAYS`] and [`MAX_ONION_LOOP_SYMBOLS`].
pub const MAX_ONION_LOOP_HOPS: usize = loop_hops(MAX_ONION_LOOP_SYMBOLS);

/// Period `s + 1` of the loop: each symbol hop `hₖ` together with the `s` relay positions before
/// it, so `hₖ` stands at position `(s + 1)·k`.
const SYMBOL_PERIOD: usize = ONION_SEGMENT_RELAYS + 1;

/// Number of positions `H(n, s) = n + (n + 1)·s = (s + 1)·n + s` of a loop over `n` symbols.
const fn loop_hops(symbols: usize) -> usize {
    SYMBOL_PERIOD * symbols + ONION_SEGMENT_RELAYS
}

/// Role of one loop position (#834 D4c, L7).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionLoopRole {
    /// The entry guard `g`, at positions `1` and `H` only; it evaluates `relay`.
    Guard,
    /// A relay other than the guard; it evaluates `relay`.
    Relay,
    /// The symbol hop `hₖ`, evaluating the `k`-th application of the pipeline (`1 ≤ k ≤ n`).
    Symbol(usize),
}

/// The shape of a loop over `n ∈ [1, n_max]` symbol applications.
///
/// Invariant: `1 ≤ symbols ≤ MAX_ONION_LOOP_SYMBOLS`, so `hop_count() ≤ MAX_ONION_LOOP_HOPS`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionLoopShape {
    symbols: usize,
}

impl OnionLoopShape {
    /// Shape of a pipeline ending in a world-facing session symbol, which stands alone (`n = 1`,
    /// #834 D4a): `g, r₀,₂, h₁, r₁,₁, g`.
    pub const SESSION: Self = Self { symbols: 1 };

    /// Build the shape of a pipeline of `symbols` applications, rejecting `n = 0` and `n > n_max`.
    pub fn new(symbols: usize) -> Result<Self> {
        if symbols == 0 || symbols > MAX_ONION_LOOP_SYMBOLS {
            return Err(Error::OnionRouteError(
                OnionRouteError::LoopSymbolsOutOfBounds {
                    symbols,
                    max_symbols: MAX_ONION_LOOP_SYMBOLS,
                },
            ));
        }
        Ok(Self { symbols })
    }

    /// Return the number `n` of symbol applications.
    pub const fn symbols(self) -> usize {
        self.symbols
    }

    /// Return the number of positions `H(n, s) = (s + 1)·n + s`.
    pub const fn hop_count(self) -> usize {
        loop_hops(self.symbols)
    }

    /// Return the number `H − 1` of pairwise distinct hops: every position once, the guard once.
    pub const fn distinct_hops(self) -> usize {
        self.hop_count() - 1
    }

    /// Return the role of position `i ∈ [1, H]`, or `None` outside the loop.
    pub const fn role(self, position: usize) -> Option<OnionLoopRole> {
        if position == 0 || position > self.hop_count() {
            None
        } else if position == 1 || position == self.hop_count() {
            Some(OnionLoopRole::Guard)
        } else if position.is_multiple_of(SYMBOL_PERIOD) {
            Some(OnionLoopRole::Symbol(position / SYMBOL_PERIOD))
        } else {
            Some(OnionLoopRole::Relay)
        }
    }

    /// Label every position by its role, stopping at the first failure.
    ///
    /// `label` is applied to the roles of the open path `1 … H − 1` in position order, so it
    /// is asked for the guard exactly once; position `H` repeats the guard's label (L7).
    pub fn try_label<P>(
        self,
        mut label: impl FnMut(OnionLoopRole) -> Result<P>,
    ) -> Result<OnionLoop<P>> {
        let guard = label(OnionLoopRole::Guard)?;
        let interior = (2..self.hop_count())
            .filter_map(|position| self.role(position))
            .map(&mut label)
            .collect::<Result<Vec<_>>>()?;
        Ok(OnionLoop {
            shape: self,
            guard,
            interior,
        })
    }
}

/// A loop whose positions each carry a label `P`, the guard's label stored once.
///
/// Invariant: `interior` labels positions `2 … H − 1` of `shape`, so `interior.len() = H − 2`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionLoop<P> {
    shape: OnionLoopShape,
    guard: P,
    interior: Vec<P>,
}

impl<P> OnionLoop<P> {
    /// Return the shape of this loop.
    pub const fn shape(&self) -> OnionLoopShape {
        self.shape
    }

    /// Return the guard's label, the label of positions `1` and `H`.
    pub const fn guard(&self) -> &P {
        &self.guard
    }

    /// Return the labels of the open path `1 … H − 1`, the guard once: the `H − 1` hops a route
    /// requires pairwise distinct.
    pub fn open_path(&self) -> impl Iterator<Item = &P> {
        iter::once(&self.guard).chain(self.interior.iter())
    }

    /// Return the labels of all `H` positions in order, the guard at `1` and `H`.
    pub fn positions(&self) -> impl Iterator<Item = &P> {
        self.open_path().chain(iter::once(&self.guard))
    }

    /// Return the label of position `i ∈ [1, H]`, or `None` outside the loop.
    pub fn position(&self, position: usize) -> Option<&P> {
        self.positions().nth(position.checked_sub(1)?)
    }

    /// Return the label of the symbol hop `hₖ`, `1 ≤ k ≤ n`, or `None` for any other `k`.
    pub fn symbol(&self, k: usize) -> Option<&P> {
        if k == 0 || k > self.shape.symbols() {
            return None;
        }
        self.position(SYMBOL_PERIOD * k)
    }
}

#[cfg(test)]
mod tests {
    use super::OnionLoopRole;
    use super::OnionLoopShape;
    use super::MAX_ONION_LOOP_HOPS;
    use super::MAX_ONION_LOOP_SYMBOLS;
    use super::ONION_SEGMENT_RELAYS;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::OnionRouteError;

    /// Size (D5): `H = (s + 1)·n + s` for every admitted `n`, `H − 1` distinct hops, and the
    /// derived bound `MAX_ONION_LOOP_HOPS = 14`.
    #[test]
    fn test_loop_size_is_derived_from_segments() -> Result<()> {
        assert_eq!(ONION_SEGMENT_RELAYS, 2);
        assert_eq!(MAX_ONION_LOOP_SYMBOLS, 4);
        assert_eq!(MAX_ONION_LOOP_HOPS, 14);
        for (symbols, hops) in [(1, 5), (2, 8), (3, 11), (4, 14)] {
            let shape = OnionLoopShape::new(symbols)?;
            assert_eq!(shape.hop_count(), hops);
            assert_eq!(
                shape.hop_count(),
                symbols + (symbols + 1) * ONION_SEGMENT_RELAYS
            );
            assert_eq!(shape.distinct_hops(), hops - 1);
            assert!(shape.hop_count() <= MAX_ONION_LOOP_HOPS);
            assert_eq!(shape.try_label(|_| Ok(()))?.positions().count(), hops);
        }
        assert_eq!(OnionLoopShape::SESSION, OnionLoopShape::new(1)?);
        Ok(())
    }

    /// `n = 0` and `n > n_max` are rejected at construction.
    #[test]
    fn test_loop_symbols_are_bounded() {
        for symbols in [0, MAX_ONION_LOOP_SYMBOLS + 1, usize::MAX] {
            assert!(matches!(
                OnionLoopShape::new(symbols),
                Err(Error::OnionRouteError(OnionRouteError::LoopSymbolsOutOfBounds {
                    symbols: rejected,
                    max_symbols: MAX_ONION_LOOP_SYMBOLS,
                })) if rejected == symbols
            ));
        }
    }

    /// Segment (D5) and guard closure (L7): the guard is at `1` and `H` only, the `k`-th symbol hop
    /// at `(s + 1)·k`, and exactly `s` relay positions, the guard counted, lie before `h₁`,
    /// between consecutive symbol hops and after `hₙ`.
    #[test]
    fn test_loop_roles_interleave_segments_of_s_relays() -> Result<()> {
        for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
            let shape = OnionLoopShape::new(symbols)?;
            let roles = (1..=shape.hop_count())
                .map(|position| shape.role(position))
                .collect::<Option<Vec<_>>>()
                .ok_or(Error::InvalidData)?;
            let guards = roles
                .iter()
                .enumerate()
                .filter(|(_, role)| **role == OnionLoopRole::Guard)
                .map(|(index, _)| index + 1)
                .collect::<Vec<_>>();
            let symbol_positions = roles
                .iter()
                .enumerate()
                .filter_map(|(index, role)| match role {
                    OnionLoopRole::Symbol(k) => Some((*k, index + 1)),
                    OnionLoopRole::Guard | OnionLoopRole::Relay => None,
                })
                .collect::<Vec<_>>();
            let segments = roles
                .split(|role| matches!(role, OnionLoopRole::Symbol(_)))
                .map(<[OnionLoopRole]>::len)
                .collect::<Vec<_>>();

            assert_eq!(guards, vec![1, shape.hop_count()]);
            assert_eq!(
                symbol_positions,
                (1..=symbols)
                    .map(|k| (k, (ONION_SEGMENT_RELAYS + 1) * k))
                    .collect::<Vec<_>>()
            );
            assert_eq!(segments, vec![ONION_SEGMENT_RELAYS; symbols + 1]);
            assert_eq!(shape.role(0), None);
            assert_eq!(shape.role(shape.hop_count() + 1), None);
        }
        Ok(())
    }

    /// Labelling asks for the guard once and closes the loop with it; the open path holds the
    /// `H − 1` distinct labels and every symbol hop sits at its role's position.
    #[test]
    fn test_try_label_closes_the_loop_with_one_guard_label() -> Result<()> {
        for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
            let shape = OnionLoopShape::new(symbols)?;
            let mut asked = Vec::new();
            let labelled = shape.try_label(|role| {
                asked.push(role);
                Ok(asked.len())
            })?;
            let positions = labelled.positions().copied().collect::<Vec<_>>();

            assert_eq!(
                asked
                    .iter()
                    .filter(|role| **role == OnionLoopRole::Guard)
                    .count(),
                1
            );
            assert_eq!(positions.first(), positions.last());
            assert_eq!(
                labelled.open_path().copied().collect::<Vec<_>>(),
                (1..shape.hop_count()).collect::<Vec<_>>()
            );
            for k in 1..=symbols {
                assert_eq!(
                    labelled.symbol(k),
                    labelled.position((ONION_SEGMENT_RELAYS + 1) * k)
                );
                assert_eq!(
                    asked.get((ONION_SEGMENT_RELAYS + 1) * k - 1),
                    Some(&OnionLoopRole::Symbol(k))
                );
            }
            assert_eq!(labelled.symbol(0), None);
            assert_eq!(labelled.symbol(symbols + 1), None);
            assert_eq!(labelled.guard(), &1);
        }
        Ok(())
    }

    /// Labelling stops at the first failure.
    #[test]
    fn test_try_label_propagates_the_first_failure() {
        let mut calls = 0;
        let labelled = OnionLoopShape::SESSION.try_label(|role| {
            calls += 1;
            match role {
                OnionLoopRole::Symbol(_) => Err(Error::InvalidData),
                OnionLoopRole::Guard | OnionLoopRole::Relay => Ok(()),
            }
        });

        assert!(matches!(labelled, Err(Error::InvalidData)));
        assert_eq!(calls, 3);
    }
}
