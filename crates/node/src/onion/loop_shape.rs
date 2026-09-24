//! Onion loops: the closed hop sequence a pipeline is evaluated on (#834 D4c, D5, L7).
//!
//! A pipeline `P = (f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ)` is evaluated on the client-sealed loop
//!
//! ```text
//! LoopOf(P):  cl → r₀,₁ … r₀,ₛ → h₁ → r₁,₁ … r₁,ₛ → ⋯ → hₙ → rₙ,₁ … rₙ,ₛ → cl
//! ```
//!
//! of `n + 1` segments of `s = ONION_SEGMENT_RELAYS` relays, closed by the client's entry guard
//! `g = r₀,₁ = rₙ,ₛ`: the guard is relay `1` of the first segment and relay `s` of the last, and
//! the symbol hop `hₖ` stands at position `(s + 1)·k` of `[1, H]`.
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
//! The only constructor is [`OnionLoop::try_unfold`], an unfold of the pipeline in the failure
//! monad along its list zipper: it walks the symbols once, emitting each segment and symbol step
//! with the symbols still pending after it,
//!
//! ```text
//! unfold [f₁ … fₙ] = Guard · Relay^{s−1} · Π_{k=1..n} (Symbol fₖ · Relay^{s − [k = n]})
//! ```
//!
//! and closes the loop with the guard's label, so the laws above hold by construction.

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
}

/// One position of the open path `1 … H − 1`, as [`OnionLoop::try_unfold`] asks for its label:
/// its kind, and the symbols of the pipeline still to be placed after it.
#[derive(Debug, Eq, PartialEq)]
pub enum OnionLoopStep<'s, S> {
    /// Position `1`, the entry guard; it evaluates `relay`.
    Guard {
        /// Every symbol of the pipeline.
        pending: &'s [S],
    },
    /// A relay position other than the guard; it evaluates `relay`.
    Relay {
        /// The symbols after this position.
        pending: &'s [S],
    },
    /// The symbol hop of `symbol`.
    Symbol {
        /// The symbol evaluated here.
        symbol: &'s S,
        /// The symbols after this position.
        pending: &'s [S],
    },
}

impl<'s, S> OnionLoopStep<'s, S> {
    /// Return the symbols still to be placed after this position.
    pub const fn pending(&self) -> &'s [S] {
        match self {
            Self::Guard { pending } | Self::Relay { pending } | Self::Symbol { pending, .. } => {
                pending
            }
        }
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
    /// Unfold the loop of the pipeline `symbols`, labelling each position of the open path
    /// `1 … H − 1` in order and stopping at the first failure.
    ///
    /// `label` is asked for the guard exactly once; position `H` repeats the guard's label (L7).
    /// A pipeline of `0` or more than `n_max` symbols is rejected before any label is asked.
    pub fn try_unfold<S>(
        symbols: &[S],
        mut label: impl FnMut(OnionLoopStep<'_, S>) -> Result<P>,
    ) -> Result<Self> {
        let shape = OnionLoopShape::new(symbols.len())?;
        let guard = label(OnionLoopStep::Guard { pending: symbols })?;
        let mut interior = Vec::with_capacity(shape.hop_count() - 2);
        for _ in 1..ONION_SEGMENT_RELAYS {
            interior.push(label(OnionLoopStep::Relay { pending: symbols })?);
        }
        let mut rest = symbols;
        while let Some((symbol, pending)) = rest.split_first() {
            interior.push(label(OnionLoopStep::Symbol { symbol, pending })?);
            let relays = if pending.is_empty() {
                ONION_SEGMENT_RELAYS - 1
            } else {
                ONION_SEGMENT_RELAYS
            };
            for _ in 0..relays {
                interior.push(label(OnionLoopStep::Relay { pending })?);
            }
            rest = pending;
        }
        Ok(Self {
            shape,
            guard,
            interior,
        })
    }

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

    /// Return the labels of positions `1 … (s + 1)·k`: the loop from the guard to the symbol
    /// hop `hₖ`, empty for `k = 0`.
    pub fn prefix_to_symbol(&self, k: usize) -> impl Iterator<Item = &P> {
        self.positions()
            .take(SYMBOL_PERIOD * k.min(self.shape.symbols()))
    }

    /// Return the label of the symbol hop `hₖ`, `1 ≤ k ≤ n`, or `None` for any other `k`.
    pub fn symbol(&self, k: usize) -> Option<&P> {
        if k == 0 || k > self.shape.symbols() {
            return None;
        }
        self.positions().nth(SYMBOL_PERIOD * k - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::OnionLoop;
    use super::OnionLoopShape;
    use super::OnionLoopStep;
    use super::MAX_ONION_LOOP_HOPS;
    use super::MAX_ONION_LOOP_SYMBOLS;
    use super::ONION_SEGMENT_RELAYS;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::OnionRouteError;

    /// The kind of one position, read off the unfolded loop: the guard, a relay, or the symbol
    /// with index `k` of the pipeline `[1 … n]`.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Kind {
        Guard,
        Relay,
        Symbol(usize),
    }

    /// Unfold the pipeline `[1 … n]`, labelling each position with its kind and with the number
    /// of symbols pending after it.
    fn unfold_kinds(symbols: usize) -> Result<OnionLoop<(Kind, usize)>> {
        let pipeline = (1..=symbols).collect::<Vec<_>>();
        OnionLoop::try_unfold(pipeline.as_slice(), |step| {
            let kind = match step {
                OnionLoopStep::Guard { .. } => Kind::Guard,
                OnionLoopStep::Relay { .. } => Kind::Relay,
                OnionLoopStep::Symbol { symbol, .. } => Kind::Symbol(*symbol),
            };
            Ok((kind, step.pending().len()))
        })
    }

    /// Size (D5): `H = (s + 1)·n + s` for every admitted `n`, `H − 1` distinct hops, and the
    /// derived bound `MAX_ONION_LOOP_HOPS = 14`.
    #[test]
    fn test_loop_size_is_derived_from_segments() -> Result<()> {
        assert_eq!(ONION_SEGMENT_RELAYS, 2);
        assert_eq!(MAX_ONION_LOOP_SYMBOLS, 4);
        assert_eq!(MAX_ONION_LOOP_HOPS, 14);
        for (symbols, hops) in [(1, 5), (2, 8), (3, 11), (4, 14)] {
            let shape = OnionLoopShape::new(symbols)?;
            let unfolded = unfold_kinds(symbols)?;
            assert_eq!(shape.hop_count(), hops);
            assert_eq!(
                shape.hop_count(),
                symbols + (symbols + 1) * ONION_SEGMENT_RELAYS
            );
            assert_eq!(shape.distinct_hops(), hops - 1);
            assert!(shape.hop_count() <= MAX_ONION_LOOP_HOPS);
            assert_eq!(unfolded.shape(), shape);
            assert_eq!(unfolded.positions().count(), hops);
            assert_eq!(unfolded.open_path().count(), hops - 1);
        }
        assert_eq!(OnionLoopShape::SESSION, OnionLoopShape::new(1)?);
        Ok(())
    }

    /// `n = 0` and `n > n_max` are rejected, before any label is asked.
    #[test]
    fn test_loop_symbols_are_bounded() -> Result<()> {
        for symbols in [0, MAX_ONION_LOOP_SYMBOLS + 1] {
            let pipeline = vec![(); symbols];
            let mut asked = 0;
            assert!(matches!(
                OnionLoop::try_unfold(pipeline.as_slice(), |_| {
                    asked += 1;
                    Ok(())
                }),
                Err(Error::OnionRouteError(OnionRouteError::LoopSymbolsOutOfBounds {
                    symbols: rejected,
                    max_symbols: MAX_ONION_LOOP_SYMBOLS,
                })) if rejected == symbols
            ));
            assert_eq!(asked, 0);
        }
        assert!(OnionLoopShape::new(usize::MAX).is_err());
        Ok(())
    }

    /// Segment (D5) and guard closure (L7): the guard is at `1` and `H` only, the `k`-th symbol at
    /// `(s + 1)·k`, exactly `s` relay positions, the guard counted, lie before `h₁`, between
    /// consecutive symbols and after `hₙ`, and every position sees the symbols after it.
    #[test]
    fn test_unfold_interleaves_segments_of_s_relays() -> Result<()> {
        for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
            let unfolded = unfold_kinds(symbols)?;
            let positions = unfolded.positions().copied().collect::<Vec<_>>();
            let kinds = positions.iter().map(|(kind, _)| *kind).collect::<Vec<_>>();
            let guards = (1..=positions.len())
                .filter(|position| kinds.get(position - 1) == Some(&Kind::Guard))
                .collect::<Vec<_>>();
            let segments = kinds
                .split(|kind| matches!(kind, Kind::Symbol(_)))
                .map(<[Kind]>::len)
                .collect::<Vec<_>>();

            assert_eq!(guards, vec![1, positions.len()]);
            for k in 1..=symbols {
                assert_eq!(
                    kinds.get((ONION_SEGMENT_RELAYS + 1) * k - 1),
                    Some(&Kind::Symbol(k))
                );
                assert_eq!(
                    unfolded.symbol(k),
                    positions.get((ONION_SEGMENT_RELAYS + 1) * k - 1)
                );
            }
            assert_eq!(segments, vec![ONION_SEGMENT_RELAYS; symbols + 1]);
            assert!(positions.iter().all(|(kind, pending)| match kind {
                Kind::Guard => *pending == symbols,
                Kind::Symbol(k) => *pending == symbols - k,
                Kind::Relay => true,
            }));
            assert_eq!(unfolded.symbol(0), None);
            assert_eq!(unfolded.symbol(symbols + 1), None);
            assert_eq!(
                unfolded.prefix_to_symbol(1).count(),
                ONION_SEGMENT_RELAYS + 1
            );
        }
        Ok(())
    }

    /// Unfolding asks for the guard once and closes the loop with it.
    #[test]
    fn test_unfold_closes_the_loop_with_one_guard_label() -> Result<()> {
        for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
            let pipeline = vec![(); symbols];
            let mut asked = 0;
            let unfolded = OnionLoop::try_unfold(pipeline.as_slice(), |_| {
                asked += 1;
                Ok(asked)
            })?;
            let positions = unfolded.positions().copied().collect::<Vec<_>>();

            assert_eq!(asked, unfolded.shape().distinct_hops());
            assert_eq!(positions.first(), positions.last());
            assert_eq!(unfolded.guard(), &1);
            assert_eq!(
                unfolded.open_path().copied().collect::<Vec<_>>(),
                (1..=unfolded.shape().distinct_hops()).collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    /// Unfolding stops at the first failure.
    #[test]
    fn test_unfold_propagates_the_first_failure() {
        let mut asked = 0;
        let unfolded = OnionLoop::<()>::try_unfold(&[()], |step| {
            asked += 1;
            match step {
                OnionLoopStep::Symbol { .. } => Err(Error::InvalidData),
                OnionLoopStep::Guard { .. } | OnionLoopStep::Relay { .. } => Ok(()),
            }
        });

        assert!(matches!(unfolded, Err(Error::InvalidData)));
        assert_eq!(asked, 3);
    }
}
