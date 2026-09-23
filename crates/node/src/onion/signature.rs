//! The onion signature `Σ`: the static table of operation symbols a circuit applies.
//!
//! A circuit is a term over `Σ` evaluated one symbol per hop (#834 D1). This phase fixes
//!
//! ```text
//! Σ = { relay, tcp, https }
//!
//! relay : X → X   identity; W(relay) = |In|, L(relay) = 0      pos(relay) = Intermediate
//! tcp             world-facing byte stream                        pos(tcp)   = WorldFacing
//! https           world-facing request/response                   pos(https) = WorldFacing
//! ```
//!
//! Laws:
//!
//! - **Identity** (#834 L1). `relay = id`, so `relay ⋙ f = f = f ⋙ relay` on carried values. The
//!   pure circuit reducer interprets `relay`; no exit adapter ever does.
//! - **Position.** A world-facing symbol exchanges bytes with the outside world and is the last
//!   application of a pipeline.
//! - **Resolution.** `spec : OnionServiceName → Σ` is total. A table name resolves to its own
//!   entry; every other canonical name is an operator-named backend of the byte-stream operation
//!   and resolves to `tcp`, because a name identifies the backend, not only the operation
//!   (#834 D1). This is the contract native exits already keep: every configured service is
//!   served at the TCP boundary.
//!
//! Width and latency classes `W`, `L` of the world-facing symbols are not protocol data yet: their
//! results return along the reversed path, never through a fixed-width carry slot.

use super::OnionRouteError;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;

/// Semantic role of a symbol; it fixes both who interprets the symbol and where it may stand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionSymbolRole {
    /// `relay = id`: output width equals input width, latency zero, interpreted by the reducer.
    Identity,
    /// Exchanges bytes with the outside world and may hold a session; interpreted by an exit
    /// adapter registered in the node's `OnionAlgebra`.
    WorldFacing,
}

impl OnionSymbolRole {
    /// Return the pipeline position `pos(f)` this role admits.
    pub const fn position(self) -> OnionSymbolPosition {
        match self {
            Self::Identity => OnionSymbolPosition::Intermediate,
            Self::WorldFacing => OnionSymbolPosition::WorldFacing,
        }
    }
}

/// Pipeline position of a symbol (#834 D1 `pos(f)`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionSymbolPosition {
    /// Followed by another application of the same pipeline.
    Intermediate,
    /// The last application of a pipeline.
    WorldFacing,
}

/// Specification `spec(f)` of one symbol of `Σ` (#834 D1).
#[derive(Debug, Eq, PartialEq)]
pub struct OnionSymbolSpec {
    name: &'static str,
    role: OnionSymbolRole,
}

impl OnionSymbolSpec {
    /// Build one table entry from its canonical name and role.
    const fn new(name: &'static str, role: OnionSymbolRole) -> Self {
        Self { name, role }
    }

    /// Return the canonical symbol name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Return the semantic role of this symbol.
    pub const fn role(&self) -> OnionSymbolRole {
        self.role
    }

    /// Return the canonical name as the symbol-name type.
    pub fn service_name(&self) -> OnionServiceName {
        OnionServiceName::static_name(self.name)
    }
}

/// The finite signature `Σ`, one named field per symbol so that every table access is total.
#[derive(Debug)]
pub struct OnionSignature {
    relay: OnionSymbolSpec,
    tcp: OnionSymbolSpec,
    https: OnionSymbolSpec,
}

/// The onion signature of this node generation.
pub static ONION_SIGNATURE: OnionSignature = OnionSignature {
    relay: OnionSymbolSpec::new("relay", OnionSymbolRole::Identity),
    tcp: OnionSymbolSpec::new("tcp", OnionSymbolRole::WorldFacing),
    https: OnionSymbolSpec::new("https", OnionSymbolRole::WorldFacing),
};

impl OnionSignature {
    /// Return the identity symbol `relay`.
    pub const fn relay(&self) -> &OnionSymbolSpec {
        &self.relay
    }

    /// Return the world-facing byte-stream symbol `tcp`.
    pub const fn tcp(&self) -> &OnionSymbolSpec {
        &self.tcp
    }

    /// Return the world-facing request symbol `https`.
    pub const fn https(&self) -> &OnionSymbolSpec {
        &self.https
    }

    /// Return `Σ` in table order.
    pub const fn symbols(&self) -> [&OnionSymbolSpec; 3] {
        [&self.relay, &self.tcp, &self.https]
    }

    /// Return the world-facing symbols of `Σ` in table order.
    pub fn world_facing(&self) -> impl Iterator<Item = &OnionSymbolSpec> {
        self.symbols()
            .into_iter()
            .filter(|spec| spec.role == OnionSymbolRole::WorldFacing)
    }

    /// Resolve a canonical name to its specification; total by the resolution law above.
    pub fn spec(&self, name: &OnionServiceName) -> &OnionSymbolSpec {
        self.symbols()
            .into_iter()
            .find(|spec| spec.name == name.as_str())
            .unwrap_or(self.tcp())
    }

    /// Resolve a name an exit registers, which must denote a world-facing symbol.
    ///
    /// `relay` is registered through the online-node relay capability, never as an exit service
    /// (#834 D2), so an exit registration naming it is rejected.
    pub fn world_facing_spec(&self, name: &OnionServiceName) -> Result<&OnionSymbolSpec> {
        let spec = self.spec(name);
        match spec.role {
            OnionSymbolRole::WorldFacing => Ok(spec),
            OnionSymbolRole::Identity => Err(Error::OnionRouteError(
                OnionRouteError::NotWorldFacingSymbol {
                    symbol: spec.name.to_string(),
                },
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnionSymbolRole;
    use super::ONION_SIGNATURE;
    use crate::onion::OnionServiceName;

    /// Every table name is canonical, and resolution of a table name returns its own entry.
    #[test]
    fn test_table_names_are_canonical_fixed_points_of_resolution() {
        for spec in ONION_SIGNATURE.symbols() {
            let name = OnionServiceName::parse(spec.name()).expect("canonical table name");
            assert_eq!(name, spec.service_name());
            assert_eq!(ONION_SIGNATURE.spec(&name), spec);
        }
    }

    /// `relay` is the only identity symbol; operator names resolve to the byte-stream symbol.
    #[test]
    fn test_resolution_is_total_and_routes_operator_names_to_tcp() {
        let identities = ONION_SIGNATURE
            .symbols()
            .into_iter()
            .filter(|spec| spec.role() == OnionSymbolRole::Identity)
            .collect::<Vec<_>>();
        let operator_name = OnionServiceName::parse("web").expect("operator name");

        assert_eq!(identities, vec![ONION_SIGNATURE.relay()]);
        assert_eq!(ONION_SIGNATURE.spec(&operator_name), ONION_SIGNATURE.tcp());
        assert_eq!(
            ONION_SIGNATURE.world_facing_spec(&operator_name).ok(),
            Some(ONION_SIGNATURE.tcp())
        );
        assert!(ONION_SIGNATURE
            .world_facing_spec(&ONION_SIGNATURE.relay().service_name())
            .is_err());
    }
}
