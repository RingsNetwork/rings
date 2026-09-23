//! Onion pipelines: closed terms over the signature `Σ`, in normal form.
//!
//! #834 D4 writes a circuit as a pipeline of applications, one per hop,
//!
//! ```text
//! P = (f₁, ā₁) ⋙ … ⋙ (fₙ, āₙ),     ⟦P⟧ = ⟦fₙ⟧(āₙ) ∘_K … ∘_K ⟦f₁⟧(ā₁)
//! ```
//!
//! where each `⟦f⟧(ā) : In_f → M Out_f` is a Kleisli arrow of the hop effect monad `M` and `∘_K` is
//! Kleisli composition. With `Σ = {relay} ⊎ Σ_W`, a world-facing symbol standing last and `relay`
//! taking no arguments (`ā = ε ≅ ()`), the well-typed pipelines are exactly
//!
//! ```text
//! relay^k ⋙ (s, ā),     k ≥ 0,  s ∈ Σ_W
//! ```
//!
//! and both types below are that normal form, so an ill-typed pipeline (an application after a
//! world-facing symbol, `relay` with arguments, no world-facing terminal) is unrepresentable:
//!
//! - [`OnionSymbolWord`] `σ = relay^k ⋙ s` is the symbol word: what a route request asks for and
//!   what a route is selected for.
//! - [`OnionPipeline<P>`] labels each position of a word with a `P`; `OnionPipeline<hop>` is the hop
//!   assignment `h₀ … hₖ` of a route.
//!
//! Substitution `σ[ā]` replaces the terminal symbol `s` by the application `(s, ā)`; relay positions
//! take `ε` and are left untouched, so the substitution happens once, at the terminal.

use super::circuit::OnionCircuitPayload;
use super::OnionRouteError;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;

/// The symbol word `σ = relay^k ⋙ s` of a closed pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionSymbolWord {
    relays: usize,
    terminal: OnionServiceName,
}

impl OnionSymbolWord {
    /// Build `relay^relays ⋙ terminal`.
    pub const fn new(relays: usize, terminal: OnionServiceName) -> Self {
        Self { relays, terminal }
    }

    /// Return the number `k` of relay positions.
    pub const fn relays(&self) -> usize {
        self.relays
    }

    /// Return the world-facing terminal symbol `s`.
    pub const fn terminal(&self) -> &OnionServiceName {
        &self.terminal
    }

    /// Return the number of positions, one hop each: `k + 1`.
    pub const fn hop_count(&self) -> usize {
        self.relays.saturating_add(1)
    }

    /// Substitute a client payload into this word: `σ[ā]`.
    ///
    /// Relay positions take `ε`, so `σ[ā]` is determined by its terminal application `(s, ā)`,
    /// which is returned. Post: the application's symbol is `s`.
    pub fn apply(&self, payload: OnionCircuitPayload) -> Result<OnionCircuitPayload> {
        if !payload.is_service(self.terminal()) {
            return Err(Error::OnionRouteError(
                OnionRouteError::PayloadServiceMismatch {
                    payload_service: payload.service().to_string(),
                    route_service: self.terminal().as_str().to_string(),
                },
            ));
        }
        Ok(payload)
    }
}

/// A closed pipeline `relay^k ⋙ s` whose `k + 1` positions each carry a label `P`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionPipeline<P> {
    relays: Vec<P>,
    terminal: P,
}

impl<P> OnionPipeline<P> {
    /// Label the relay positions with `relays` and the world-facing position with `terminal`.
    pub const fn new(relays: Vec<P>, terminal: P) -> Self {
        Self { relays, terminal }
    }

    /// Return the labels of the relay positions, in evaluation order.
    pub fn relays(&self) -> &[P] {
        self.relays.as_slice()
    }

    /// Return the label of the world-facing position.
    pub const fn terminal(&self) -> &P {
        &self.terminal
    }

    /// Return the label of position zero, the first hop.
    pub fn first(&self) -> &P {
        self.relays.first().unwrap_or(&self.terminal)
    }

    /// Return the number of positions, one hop each.
    pub fn hop_count(&self) -> usize {
        self.relays.len().saturating_add(1)
    }

    /// Relabel every position in evaluation order, stopping at the first failure.
    ///
    /// The shape is preserved: relabelling changes labels, never `k`.
    pub fn try_map<Q>(self, mut label: impl FnMut(P) -> Result<Q>) -> Result<OnionPipeline<Q>> {
        let relays = self
            .relays
            .into_iter()
            .map(&mut label)
            .collect::<Result<Vec<_>>>()?;
        Ok(OnionPipeline {
            relays,
            terminal: label(self.terminal)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::OnionPipeline;
    use super::OnionSymbolWord;
    use crate::error::Error;
    use crate::error::Result;
    use crate::onion::circuit::OnionCircuitPayload;
    use crate::onion::OnionRouteError;
    use crate::onion::OnionServiceName;

    /// Substitution admits exactly the payloads naming the word's terminal symbol.
    #[test]
    fn test_substitution_happens_at_the_terminal_symbol() -> Result<()> {
        let word = OnionSymbolWord::new(2, OnionServiceName::https());
        let payload = OnionCircuitPayload::new(OnionServiceName::https(), Bytes::from_static(b"a"));

        assert_eq!(word.hop_count(), 3);
        assert_eq!(word.apply(payload.clone())?, payload);
        assert!(matches!(
            word.apply(OnionCircuitPayload::new(
                OnionServiceName::tcp(),
                Bytes::new()
            )),
            Err(Error::OnionRouteError(
                OnionRouteError::PayloadServiceMismatch { .. }
            ))
        ));
        Ok(())
    }

    /// Relabelling visits positions in evaluation order and preserves the shape.
    #[test]
    fn test_try_map_preserves_shape_and_order() -> Result<()> {
        let mut visited = Vec::new();
        let relabelled = OnionPipeline::new(vec![1, 2], 3).try_map(|label| {
            visited.push(label);
            Ok(label * 10)
        })?;

        assert_eq!(visited, vec![1, 2, 3]);
        assert_eq!(relabelled, OnionPipeline::new(vec![10, 20], 30));
        assert_eq!(*relabelled.first(), 10);
        assert_eq!(*OnionPipeline::new(Vec::new(), 7).first(), 7);
        Ok(())
    }
}
