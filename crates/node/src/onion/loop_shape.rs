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
//!   `hₖ` from `hₖ₊₁` and follow `hₙ`; the only constructor emits no other segment length.
//! - **Guard closure** (L7). An [`OnionLoop`] stores the guard once and places it at positions `1`
//!   and `H` and nowhere else, so the guard is the only hop adjacent to the client. The remaining
//!   `H − 2` positions are pairwise distinct and distinct from `g` for every loop a route admits
//!   (`has_duplicate_dids` in the route module).
//!
//! The only constructor is [`OnionLoop::try_unfold`], an unfold of the non-empty, already
//! labelled pipeline `f₁ … fₙ₋₁ ⋙ fₙ` in the failure monad: it places each symbol and labels the
//! relay positions around them,
//!
//! ```text
//! unfold [f₁ … fₙ] = Guard · Relay^{s−1} · Π_{k=1..n} (Symbol fₖ · Relay^{s − [k = n]})
//! ```
//!
//! and closes the loop with the guard's label. Relay positions are labelled `P` and symbol
//! positions `T`; the terminal `hₙ` is a field, not an index, so a caller labelling it with the
//! drawn registrant reads it back totally. The unfold fixes the position order only: route
//! selection draws the symbols first, then the guard, then the relays (#834 L7).

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

/// A non-empty pipeline of symbols `f₁ … fₙ` (#834 D4a): every constructor proves `n ≥ 1`, so
/// the terminal symbol `fₙ` is total.
#[derive(Debug)]
pub(crate) struct OnionPipelineSymbols<'s, S> {
    all: &'s [S],
    intermediate: &'s [S],
    terminal: &'s S,
}

impl<'s, S> OnionPipelineSymbols<'s, S> {
    /// The pipeline of one symbol.
    pub(crate) const fn single(symbol: &'s S) -> Self {
        Self {
            all: std::slice::from_ref(symbol),
            intermediate: &[],
            terminal: symbol,
        }
    }

    /// The pipeline `symbols`, or `None` when it is empty. Multi-symbol pipelines are drawn only in
    /// tests until Phase 2b registers intermediate symbols.
    #[cfg(test)]
    pub(crate) fn new(symbols: &'s [S]) -> Option<Self> {
        symbols.split_last().map(|(terminal, intermediate)| Self {
            all: symbols,
            intermediate,
            terminal,
        })
    }

    /// Return the number `n ≥ 1` of symbols.
    pub(crate) const fn symbol_count(&self) -> usize {
        self.all.len()
    }

    /// Return the symbols in pipeline order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &'s S> {
        self.all.iter()
    }

    /// Label every symbol in pipeline order, handing each the symbols after it, and stop at the
    /// first failure: `fₖ ↦ label(fₖ, [fₖ₊₁ … fₙ])`, split into the intermediate labels and the
    /// terminal's.
    pub(crate) fn try_map_with_later<T>(
        &self,
        mut label: impl FnMut(&'s S, &'s [S]) -> Result<T>,
    ) -> Result<(Vec<T>, T)> {
        let laters = iter::successors(self.all.split_first().map(|(_, later)| later), |later| {
            later.split_first().map(|(_, rest)| rest)
        });
        let intermediate = self
            .intermediate
            .iter()
            .zip(laters)
            .map(|(symbol, later)| label(symbol, later))
            .collect::<Result<Vec<_>>>()?;
        Ok((intermediate, label(self.terminal, &[])?))
    }
}

/// The kind of a relay position of the open path: the entry guard at position `1`, or another
/// relay. Both evaluate `relay`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnionLoopRelay {
    /// Position `1`, the entry guard `g`.
    Guard,
    /// A relay position other than the guard.
    Relay,
}

/// A loop whose relay positions carry a label `P` and whose symbol positions carry a label `T`,
/// the guard's label stored once.
///
/// The fields follow the unfold grammar of the module documentation: `lead` holds the relays
/// after the guard, `stages` each intermediate symbol with the relays after it, `terminal` the
/// last symbol `hₙ`, and `tail` the relays before the closing guard. The guard and the terminal
/// are total, and the shape is derived from `stages`. The segment lengths `s − 1`, `s`, `s − 1`
/// are established by `OnionLoop::try_unfold`, the only constructor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionLoop<P, T = P> {
    guard: P,
    lead: Vec<P>,
    stages: Vec<(T, Vec<P>)>,
    terminal: T,
    tail: Vec<P>,
}

impl<P, T> OnionLoop<P, T> {
    /// Unfold the loop of the labelled symbols `intermediate ⋙ terminal`, placing each symbol at
    /// its position and labelling every relay position of the open path `1 … H − 1` in order,
    /// stopping at the first failure.
    ///
    /// The guard is labelled exactly once, and position `H` repeats its label (L7). A pipeline
    /// of more than `n_max` symbols is rejected before any relay is labelled.
    pub(crate) fn try_unfold(
        intermediate: Vec<T>,
        terminal: T,
        mut label_relay: impl FnMut(OnionLoopRelay) -> Result<P>,
    ) -> Result<Self> {
        OnionLoopShape::new(intermediate.len() + 1)?;
        let guard = label_relay(OnionLoopRelay::Guard)?;
        let lead = (1..ONION_SEGMENT_RELAYS)
            .map(|_| label_relay(OnionLoopRelay::Relay))
            .collect::<Result<Vec<_>>>()?;
        let stages = intermediate
            .into_iter()
            .map(|symbol| {
                let relays = (0..ONION_SEGMENT_RELAYS)
                    .map(|_| label_relay(OnionLoopRelay::Relay))
                    .collect::<Result<Vec<_>>>()?;
                Ok((symbol, relays))
            })
            .collect::<Result<Vec<_>>>()?;
        let tail = (1..ONION_SEGMENT_RELAYS)
            .map(|_| label_relay(OnionLoopRelay::Relay))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            guard,
            lead,
            stages,
            terminal,
            tail,
        })
    }

    /// Return the shape of this loop: `n = |stages| + 1` symbols, bounded by construction.
    pub fn shape(&self) -> OnionLoopShape {
        OnionLoopShape {
            symbols: self.stages.len() + 1,
        }
    }

    /// Return the guard's label, the label of positions `1` and `H`.
    pub const fn guard(&self) -> &P {
        &self.guard
    }

    /// Return the label of the terminal symbol hop `hₙ`.
    pub const fn terminal(&self) -> &T {
        &self.terminal
    }

    /// Return the labels of the relay positions before `h₁`: the guard, then `r₀,₂ … r₀,ₛ`.
    pub fn forward_prefix(&self) -> impl Iterator<Item = &P> {
        iter::once(&self.guard).chain(self.lead.iter())
    }

    /// Relabel every symbol position by `project` and return the terminal's label beside the
    /// relabelled loop: `OnionLoop<P, T> → OnionLoop<P> × T`.
    pub(crate) fn project_symbols(self, project: impl Fn(&T) -> P) -> (OnionLoop<P>, T) {
        let stages = self
            .stages
            .into_iter()
            .map(|(symbol, relays)| (project(&symbol), relays))
            .collect();
        (
            OnionLoop {
                guard: self.guard,
                lead: self.lead,
                stages,
                terminal: project(&self.terminal),
                tail: self.tail,
            },
            self.terminal,
        )
    }
}

impl<P> OnionLoop<P> {
    /// Return the labels of the open path `1 … H − 1`, the guard once: the `H − 1` hops a route
    /// requires pairwise distinct.
    pub fn open_path(&self) -> impl Iterator<Item = &P> {
        self.forward_prefix()
            .chain(
                self.stages
                    .iter()
                    .flat_map(|(symbol, relays)| iter::once(symbol).chain(relays.iter())),
            )
            .chain(iter::once(&self.terminal))
            .chain(self.tail.iter())
    }

    /// Return the labels of all `H` positions in order, the guard at `1` and `H`.
    pub fn positions(&self) -> impl Iterator<Item = &P> {
        self.open_path().chain(iter::once(&self.guard))
    }

    /// Return the label of the symbol hop `hₖ`, `1 ≤ k ≤ n`, or `None` for any other `k`.
    pub fn symbol(&self, k: usize) -> Option<&P> {
        if k == self.shape().symbols() {
            return Some(&self.terminal);
        }
        self.stages.get(k.checked_sub(1)?).map(|(symbol, _)| symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::OnionLoop;
    use super::OnionLoopRelay;
    use super::OnionLoopShape;
    use super::OnionPipelineSymbols;
    use super::MAX_ONION_LOOP_HOPS;
    use super::MAX_ONION_LOOP_SYMBOLS;
    use super::ONION_SEGMENT_RELAYS;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::OnionRouteError;

    /// The kind of one position, read off the unfolded loop: the guard, a relay, or the symbol
    /// `k` of the pipeline `[1 … n]`.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Kind {
        Guard,
        Relay,
        Symbol(usize),
    }

    /// Unfold the pipeline `[1 … n]` with each position labelled by its kind, and count the relay
    /// labels asked for.
    fn unfold_kinds(symbols: usize) -> Result<(OnionLoop<Kind>, usize)> {
        let mut asked = 0;
        let unfolded = OnionLoop::try_unfold(
            (1..symbols).map(Kind::Symbol).collect(),
            Kind::Symbol(symbols),
            |relay| {
                asked += 1;
                Ok(match relay {
                    OnionLoopRelay::Guard => Kind::Guard,
                    OnionLoopRelay::Relay => Kind::Relay,
                })
            },
        )?;
        Ok((unfolded, asked))
    }

    /// Size (D5): `H = (s + 1)·n + s` for every admitted `n`, `H − 1` distinct hops asked for once
    /// each, and the derived bound `MAX_ONION_LOOP_HOPS = 14`.
    #[test]
    fn test_loop_size_is_derived_from_segments() -> Result<()> {
        assert_eq!(ONION_SEGMENT_RELAYS, 2);
        assert_eq!(MAX_ONION_LOOP_SYMBOLS, 4);
        assert_eq!(MAX_ONION_LOOP_HOPS, 14);
        for (symbols, hops) in [(1, 5), (2, 8), (3, 11), (4, 14)] {
            let shape = OnionLoopShape::new(symbols)?;
            let (unfolded, asked) = unfold_kinds(symbols)?;
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
            assert_eq!(asked + symbols, hops - 1);
        }
        assert_eq!(OnionLoopShape::SESSION, OnionLoopShape::new(1)?);
        Ok(())
    }

    /// `n = 0` and `n > n_max` are rejected, the latter before any label is asked.
    #[test]
    fn test_loop_symbols_are_bounded() {
        let mut asked = 0;
        let unfolded =
            OnionLoop::<(), ()>::try_unfold(vec![(); MAX_ONION_LOOP_SYMBOLS], (), |_| {
                asked += 1;
                Ok(())
            });

        assert!(matches!(
            unfolded,
            Err(Error::OnionRouteError(
                OnionRouteError::LoopSymbolsOutOfBounds {
                    symbols: 5,
                    max_symbols: MAX_ONION_LOOP_SYMBOLS,
                }
            ))
        ));
        assert_eq!(asked, 0);
        for symbols in [0, usize::MAX] {
            assert!(OnionLoopShape::new(symbols).is_err());
        }
    }

    /// Segment (D5) and guard closure (L7): the guard is at `1` and `H` only, the `k`-th symbol at
    /// `(s + 1)·k`, exactly `s` relay positions, the guard counted, lie before `h₁`, between
    /// consecutive symbols and after `hₙ`.
    #[test]
    fn test_unfold_interleaves_segments_of_s_relays() -> Result<()> {
        for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
            let (unfolded, _) = unfold_kinds(symbols)?;
            let kinds = unfolded.positions().copied().collect::<Vec<_>>();
            let guards = (1..=kinds.len())
                .filter(|position| kinds.get(position - 1) == Some(&Kind::Guard))
                .collect::<Vec<_>>();
            let segments = kinds
                .split(|kind| matches!(kind, Kind::Symbol(_)))
                .map(<[Kind]>::len)
                .collect::<Vec<_>>();

            assert_eq!(guards, vec![1, kinds.len()]);
            for k in 1..=symbols {
                assert_eq!(
                    kinds.get((ONION_SEGMENT_RELAYS + 1) * k - 1),
                    Some(&Kind::Symbol(k))
                );
                assert_eq!(
                    unfolded.symbol(k),
                    kinds.get((ONION_SEGMENT_RELAYS + 1) * k - 1)
                );
            }
            assert_eq!(segments, vec![ONION_SEGMENT_RELAYS; symbols + 1]);
            assert_eq!(unfolded.guard(), &Kind::Guard);
            assert_eq!(unfolded.terminal(), &Kind::Symbol(symbols));
            assert_eq!(unfolded.symbol(0), None);
            assert_eq!(unfolded.symbol(symbols + 1), None);
            assert_eq!(
                unfolded.forward_prefix().copied().collect::<Vec<_>>(),
                kinds
                    .get(..ONION_SEGMENT_RELAYS)
                    .map(<[Kind]>::to_vec)
                    .unwrap_or_default()
            );
        }
        Ok(())
    }

    /// Projecting the symbol labels keeps every position and hands back the terminal's label.
    #[test]
    fn test_project_symbols_returns_the_terminal_label() -> Result<()> {
        let unfolded = OnionLoop::<u64, u64>::try_unfold(vec![1000], 2000, |relay| {
            Ok(match relay {
                OnionLoopRelay::Guard => 1,
                OnionLoopRelay::Relay => 2,
            })
        })?;

        let (projected, terminal) = unfolded.project_symbols(|label| label / 100 + 5);

        assert_eq!(terminal, 2000);
        assert_eq!(projected.positions().copied().collect::<Vec<_>>(), vec![
            1, 2, 15, 2, 2, 25, 2, 1
        ]);
        Ok(())
    }

    /// Unfolding stops at the first failure.
    #[test]
    fn test_unfold_propagates_the_first_failure() {
        let mut asked = 0;
        let unfolded = OnionLoop::<(), ()>::try_unfold(Vec::new(), (), |_| {
            asked += 1;
            if asked > ONION_SEGMENT_RELAYS {
                Err(Error::InvalidData)
            } else {
                Ok(())
            }
        });

        assert!(matches!(unfolded, Err(Error::InvalidData)));
        assert_eq!(asked, ONION_SEGMENT_RELAYS + 1);
    }

    /// Each symbol is labelled with the symbols after it, in pipeline order, the terminal with none.
    #[test]
    fn test_symbols_are_labelled_with_the_symbols_after_them() -> Result<()> {
        let pipeline = [1, 2, 3];
        let symbols = OnionPipelineSymbols::new(&pipeline).ok_or(Error::InvalidData)?;

        let labelled = symbols.try_map_with_later(|symbol, later| Ok((*symbol, later.to_vec())))?;

        assert_eq!(
            labelled,
            (vec![(1, vec![2, 3]), (2, vec![3])], (3, Vec::new()))
        );
        assert_eq!(symbols.symbol_count(), 3);
        assert!(OnionPipelineSymbols::<u8>::new(&[]).is_none());
        assert_eq!(OnionPipelineSymbols::single(&7).symbol_count(), 1);
        Ok(())
    }
}
