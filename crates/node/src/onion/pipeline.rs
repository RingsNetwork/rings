//! Onion pipelines: typed terms over the signature `Σ`.
//!
//! A pipeline is a non-empty sequence of applications (#834 D3, D4)
//!
//! ```text
//! P = (f₁, ā₁) ⋙ (f₂, ā₂) ⋙ … ⋙ (fₙ, āₙ),   n ≥ 1,   fᵢ ∈ Σ
//! ⟦P⟧ = ⟦fₙ⟧(āₙ) ∘ … ∘ ⟦f₁⟧(ā₁)              (Kleisli composition in the exit monad)
//! ```
//!
//! evaluated one application per hop. `⋙` type-checks against [`ONION_SIGNATURE`]:
//!
//! - the identity symbol takes no arguments: `ā = ε` for `relay`;
//! - nothing follows a world-facing application, so a world-facing symbol stands last.
//!
//! Every carried value is an opaque byte string, so `Out(fᵢ) = In(fᵢ₊₁)` holds by construction
//! and position is the only constraint `⋙` can violate. `⋙` appends one application, so a
//! pipeline is a word of the free semigroup on applications restricted to well-positioned words,
//! and composition is associative on those words (#834 L2).
//!
//! A pipeline is *closed* when its last symbol is world-facing. Because `relay` is the only
//! non-world-facing symbol of `Σ`, the closed pipelines are exactly `relay^k ⋙ (s, ā)`: today's
//! circuits, one application per hop, encoded by the wire without change.

use std::iter;

use bytes::Bytes;

use super::signature::OnionSymbolPosition;
use super::signature::OnionSymbolRole;
use super::signature::OnionSymbolSpec;
use super::signature::ONION_SIGNATURE;
use super::OnionRouteError;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;

/// One application `(f, ā)` of a symbol of `Σ` (#834 D3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionApplication {
    symbol: OnionServiceName,
    args: Bytes,
}

impl OnionApplication {
    /// Return the identity application `(relay, ε)`.
    pub fn relay() -> Self {
        Self {
            symbol: ONION_SIGNATURE.relay().service_name(),
            args: Bytes::new(),
        }
    }

    /// Apply `symbol` to client-supplied `args`.
    ///
    /// Pre: none. Post: an identity symbol is applied to `ε` only.
    pub fn new(symbol: OnionServiceName, args: impl Into<Bytes>) -> Result<Self> {
        let application = Self {
            symbol,
            args: args.into(),
        };
        match (application.spec().role(), application.args.is_empty()) {
            (OnionSymbolRole::Identity, false) => Err(Error::OnionRouteError(
                OnionRouteError::ArgumentsToIdentitySymbol,
            )),
            _ => Ok(application),
        }
    }

    /// Return the applied symbol.
    pub fn symbol(&self) -> &OnionServiceName {
        &self.symbol
    }

    /// Return the client-supplied arguments.
    pub fn args(&self) -> &Bytes {
        &self.args
    }

    /// Return the specification of the applied symbol.
    pub fn spec(&self) -> &'static OnionSymbolSpec {
        ONION_SIGNATURE.spec(self.symbol())
    }

    /// Return whether this application may only stand last.
    fn is_world_facing(&self) -> bool {
        self.spec().role().position() == OnionSymbolPosition::WorldFacing
    }
}

/// A well-typed pipeline over `Σ` (see the module laws).
///
/// Invariant: `applications` is non-empty and no application follows a world-facing one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionPipeline {
    applications: Vec<OnionApplication>,
}

impl OnionPipeline {
    /// Return the one-application pipeline `η(f, ā)`.
    pub fn new(application: OnionApplication) -> Self {
        Self {
            applications: vec![application],
        }
    }

    /// Compose `self ⋙ next`, rejecting any application after a world-facing symbol.
    pub fn then(mut self, next: OnionApplication) -> Result<Self> {
        if let Some(last) = self
            .applications
            .last()
            .filter(|last| last.is_world_facing())
        {
            return Err(Error::OnionRouteError(
                OnionRouteError::WorldFacingSymbolNotLast {
                    symbol: last.symbol().as_str().to_string(),
                },
            ));
        }
        self.applications.push(next);
        Ok(self)
    }

    /// Build the closed normal form `relay^relays ⋙ world_facing` as a left fold of `⋙`.
    ///
    /// Pre: the caller bounds `relays` (one application per hop). Post: the result is closed, or
    /// the call fails because `world_facing` is not world-facing.
    pub fn relayed(relays: usize, world_facing: OnionApplication) -> Result<Self> {
        let mut applications = iter::repeat_with(OnionApplication::relay)
            .take(relays)
            .chain(iter::once(world_facing));
        let Some(first) = applications.next() else {
            return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
        };
        let pipeline = applications.try_fold(Self::new(first), Self::then)?;
        pipeline.closed()?;
        Ok(pipeline)
    }

    /// Return the applications in evaluation order.
    pub fn applications(&self) -> &[OnionApplication] {
        self.applications.as_slice()
    }

    /// Split a closed pipeline into its intermediate prefix and its world-facing application.
    pub fn closed(&self) -> Result<(&[OnionApplication], &OnionApplication)> {
        match self.applications.split_last() {
            Some((last, prefix)) if last.is_world_facing() => Ok((prefix, last)),
            Some((last, _)) => Err(Error::OnionRouteError(
                OnionRouteError::NotWorldFacingSymbol {
                    symbol: last.symbol().as_str().to_string(),
                },
            )),
            None => Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnionApplication;
    use super::OnionPipeline;
    use crate::error::Error;
    use crate::onion::OnionRouteError;
    use crate::onion::OnionServiceName;

    /// Build one world-facing application of the `https` symbol.
    fn https(body: &'static [u8]) -> OnionApplication {
        OnionApplication::new(OnionServiceName::https(), body).expect("https application")
    }

    /// `relay^k ⋙ w` is the left fold of `⋙` and is closed at `w`.
    #[test]
    fn test_relayed_is_the_left_fold_of_then() -> crate::error::Result<()> {
        let folded = OnionPipeline::new(OnionApplication::relay())
            .then(OnionApplication::relay())?
            .then(https(b"body"))?;
        let normal = OnionPipeline::relayed(2, https(b"body"))?;
        let (prefix, last) = normal.closed()?;

        assert_eq!(normal, folded);
        assert_eq!(prefix, [
            OnionApplication::relay(),
            OnionApplication::relay()
        ]);
        assert_eq!(last, &https(b"body"));
        assert_eq!(OnionPipeline::relayed(0, https(b"body"))?.applications(), [
            https(b"body")
        ]);
        Ok(())
    }

    /// Nothing follows a world-facing symbol, and the identity symbol takes no arguments.
    #[test]
    fn test_then_rejects_ill_positioned_and_ill_applied_terms() {
        assert!(matches!(
            OnionPipeline::new(https(b"body")).then(OnionApplication::relay()),
            Err(Error::OnionRouteError(
                OnionRouteError::WorldFacingSymbolNotLast { .. }
            ))
        ));
        assert!(matches!(
            OnionApplication::new(
                OnionServiceName::parse("relay").expect("name"),
                b"x".as_slice()
            ),
            Err(Error::OnionRouteError(
                OnionRouteError::ArgumentsToIdentitySymbol
            ))
        ));
    }

    /// An open pipeline `relay^k` has no world-facing symbol to close it.
    #[test]
    fn test_relayed_rejects_an_identity_terminal() {
        assert!(matches!(
            OnionPipeline::relayed(1, OnionApplication::relay()),
            Err(Error::OnionRouteError(
                OnionRouteError::NotWorldFacingSymbol { .. }
            ))
        ));
    }
}
