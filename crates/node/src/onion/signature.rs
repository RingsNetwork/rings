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
//! - **Closure.** `Σ` is closed and [`OnionServiceName`] is exactly its set of names:
//!   `OnionServiceName ≅ Σ`. Parsing is the only way in, so a name outside the table is rejected
//!   wherever it enters the node (configuration, descriptor decode, RPC) and no route, layer or
//!   algebra entry can name it. Resolution `spec : OnionServiceName → Σ` is therefore a total
//!   projection, never a lookup with a fallback. The encoding is the canonical name string, so
//!   closure changes no wire byte.
//!
//! Width and latency classes `W`, `L` of the world-facing symbols are not protocol data yet: their
//! results return along the reversed path, never through a fixed-width carry slot.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use super::OnionRouteError;
use crate::error::Error;
use crate::error::Result;

/// Semantic role of a symbol; it fixes both who interprets the symbol and where it may stand.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

/// Specification `spec(f)` of one symbol of `Σ` (#834 D1), ordered by name first.
#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

    /// Return the name of this table entry.
    pub fn service_name(&'static self) -> OnionServiceName {
        OnionServiceName(self)
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
}

/// Canonical name of a symbol of `Σ`: the closed name type of onion services.
///
/// Invariant: every value denotes one entry of [`ONION_SIGNATURE`] (see the closure law). It is
/// encoded as that entry's canonical name string.
#[derive(Clone, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionServiceName(&'static OnionSymbolSpec);

impl OnionServiceName {
    /// Parse and canonicalize a service name, admitting exactly the names of `Σ`.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed != name {
            return Err(Error::InvalidConfig(
                "onion exit service name must be non-empty and trimmed".to_string(),
            ));
        }
        ONION_SIGNATURE
            .symbols()
            .into_iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(trimmed))
            .map(OnionSymbolSpec::service_name)
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "unknown onion service {name:?}; the onion signature is closed: expected one of {}",
                    ONION_SIGNATURE
                        .symbols()
                        .map(OnionSymbolSpec::name)
                        .join(", ")
                ))
            })
    }

    /// Return the name of the world-facing `https` symbol.
    pub fn https() -> Self {
        ONION_SIGNATURE.https().service_name()
    }

    /// Return the name of the world-facing `tcp` symbol.
    pub fn tcp() -> Self {
        ONION_SIGNATURE.tcp().service_name()
    }

    /// Return the canonical name as a string slice.
    pub fn as_str(&self) -> &'static str {
        self.0.name
    }

    /// Return the specification of the named symbol; total by the closure law.
    pub fn spec(&self) -> &'static OnionSymbolSpec {
        self.0
    }

    /// Return the specification of a name an exit registers, which must be world-facing.
    ///
    /// `relay` is registered through the online-node relay capability, never as an exit service
    /// (#834 D2), so an exit registration naming it is rejected.
    pub fn world_facing_spec(&self) -> Result<&'static OnionSymbolSpec> {
        match self.0.role {
            OnionSymbolRole::WorldFacing => Ok(self.0),
            OnionSymbolRole::Identity => Err(Error::OnionRouteError(
                OnionRouteError::NotWorldFacingSymbol {
                    symbol: self.0.name.to_string(),
                },
            )),
        }
    }

    /// Return whether this name equals `service` after service-name canonicalization.
    pub fn matches(&self, service: &str) -> bool {
        Self::parse(service).is_ok_and(|candidate| candidate == *self)
    }
}

impl fmt::Debug for OnionServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnionServiceName")
            .field(&self.0.name)
            .finish()
    }
}

impl TryFrom<String> for OnionServiceName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionServiceName> for String {
    fn from(name: OnionServiceName) -> Self {
        name.as_str().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::OnionServiceName;
    use super::OnionSymbolRole;
    use super::ONION_SIGNATURE;

    /// Every table name parses to its own entry, case-insensitively, and nothing else parses.
    #[test]
    fn test_names_are_exactly_the_closed_signature() {
        for spec in ONION_SIGNATURE.symbols() {
            let name = OnionServiceName::parse(spec.name()).expect("table name");
            assert_eq!(name.spec(), spec);
            assert_eq!(
                OnionServiceName::parse(spec.name().to_ascii_uppercase()).ok(),
                Some(name)
            );
        }
        for outside in ["web", "api", "custom", "tcp2", "", " tcp", "tcp!"] {
            assert!(OnionServiceName::parse(outside).is_err());
        }
    }

    /// Decoding admits only names of `Σ`, and the encoding is the bare canonical name string.
    #[test]
    fn test_codec_is_the_name_string_and_rejects_names_outside_the_signature() {
        let encoded = rings_codec::serialize(&OnionServiceName::https()).expect("encode name");
        let web = rings_codec::serialize(&"web").expect("encode string");

        assert_eq!(
            encoded,
            rings_codec::serialize(&"https").expect("encode string")
        );
        assert!(rings_codec::deserialize::<OnionServiceName>(web.as_slice()).is_err());
    }

    /// `relay` is the only identity symbol and is never a world-facing registration.
    #[test]
    fn test_relay_is_the_only_identity_and_not_world_facing() {
        let identities = ONION_SIGNATURE
            .symbols()
            .into_iter()
            .filter(|spec| spec.role() == OnionSymbolRole::Identity)
            .collect::<Vec<_>>();

        assert_eq!(identities, vec![ONION_SIGNATURE.relay()]);
        assert!(ONION_SIGNATURE
            .relay()
            .service_name()
            .world_facing_spec()
            .is_err());
        assert_eq!(
            OnionServiceName::tcp().world_facing_spec().ok(),
            Some(ONION_SIGNATURE.tcp())
        );
    }
}
