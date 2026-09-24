//! The onion signature `Σ`: the static table of operation symbols a circuit applies.
//!
//! A circuit is a term over `Σ` evaluated one symbol per hop (#834 D1). This phase fixes
//!
//! ```text
//! Σ   = {relay} ⊎ Σ_W,    Σ_W = {tcp, https}
//!
//! relay : X → X   identity; L(relay) = 0, preserves the carried width    pos = Intermediate
//! tcp             world-facing byte stream                                pos = WorldFacing
//! https           world-facing request/response (fetch)                  pos = WorldFacing
//! ```
//!
//! Laws:
//!
//! - **Identity** (#834 L1). `relay = id`, so `relay ⋙ f = f` on carried values. The pure circuit
//!   reducer interprets `relay`; no exit adapter ever does, and `relay` is registered through the
//!   online-node relay capability, never as a service (#834 D2).
//! - **Position.** A world-facing symbol exchanges bytes with the outside world and stands last.
//! - **Closure.** `Σ` is closed and [`OnionServiceName`] is exactly `Σ_W`: `OnionServiceName ≅ Σ_W`.
//!   Parsing is the only way in, so a name outside `Σ_W`, `relay` included, is rejected wherever it
//!   enters the node (configuration, descriptor decode, RPC), and no route, exit layer or algebra
//!   entry can name it. The encoding is the canonical name string, so closure changes no wire byte.
//! - **Refinement.** `https ⊑ tcp`: a fetch is one request/response exchange that a byte stream can
//!   carry, so every exit able to interpret `tcp` can interpret `https`, while a browser exit, which
//!   has `fetch` but no sockets, interprets `https` alone.
//!
//! - **Code.** The `f` byte of the uniform layer (#834 D6″) is a symbol's position in table order,
//!   `code : Σ ↪ u8`, and `by_code` is its left inverse: `by_code ∘ code = Some`.
//!
//! Width and latency classes `W`, `L` of the world-facing symbols are not protocol data yet: their
//! results return along the reversed path, never through a fixed-width carry slot.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;

/// Pipeline position `pos(f)` of a symbol (#834 D1); it also fixes who interprets the symbol.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum OnionSymbolPosition {
    /// Followed by another application: the identity `relay`, interpreted by the reducer.
    Intermediate,
    /// The last application: exchanges bytes with the outside world, interpreted by an exit
    /// adapter registered in the node's `OnionAlgebra`.
    WorldFacing,
}

/// Specification `spec(f)` of one symbol of `Σ` (#834 D1), ordered by name first.
#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OnionSymbolSpec {
    name: &'static str,
    position: OnionSymbolPosition,
}

impl OnionSymbolSpec {
    /// Build one table entry from its canonical name and position.
    const fn new(name: &'static str, position: OnionSymbolPosition) -> Self {
        Self { name, position }
    }

    /// Return the canonical symbol name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Return the pipeline position of this symbol.
    pub const fn position(&self) -> OnionSymbolPosition {
        self.position
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
    relay: OnionSymbolSpec::new("relay", OnionSymbolPosition::Intermediate),
    tcp: OnionSymbolSpec::new("tcp", OnionSymbolPosition::WorldFacing),
    https: OnionSymbolSpec::new("https", OnionSymbolPosition::WorldFacing),
};

impl OnionSignature {
    /// Return the identity symbol `relay`.
    pub const fn relay(&self) -> &OnionSymbolSpec {
        &self.relay
    }

    /// Return `Σ` in table order.
    pub const fn symbols(&self) -> [&OnionSymbolSpec; 3] {
        [&self.relay, &self.tcp, &self.https]
    }

    /// Return the layer code of `spec`: the number of symbols before it in table order.
    pub fn code(&self, spec: &OnionSymbolSpec) -> u8 {
        self.symbols()
            .into_iter()
            .take_while(|symbol| *symbol != spec)
            .fold(0, |code, _| code.saturating_add(1))
    }

    /// Return the symbol whose layer code is `code`, if any, as the coproduct
    /// `Σ = {relay} ⊎ Σ_W`: the left inverse of [`Self::code`].
    ///
    /// `relay` is the only intermediate symbol, so every other entry of `Σ` names `Σ_W`.
    pub fn by_code(&'static self, code: u8) -> Option<OnionSymbol> {
        self.symbols()
            .into_iter()
            .nth(usize::from(code))
            .map(|spec| match spec.position {
                OnionSymbolPosition::Intermediate => OnionSymbol::Relay,
                OnionSymbolPosition::WorldFacing => {
                    OnionSymbol::WorldFacing(OnionServiceName(spec))
                }
            })
    }

    /// Return `Σ_W`, the world-facing symbols of `Σ`, in table order as service names.
    pub fn world_facing(&'static self) -> impl Iterator<Item = OnionServiceName> {
        self.symbols()
            .into_iter()
            .filter(|spec| spec.position == OnionSymbolPosition::WorldFacing)
            .map(OnionServiceName)
    }
}

/// A symbol of `Σ = {relay} ⊎ Σ_W`, as that coproduct.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionSymbol {
    /// The identity symbol `relay`, the only intermediate symbol.
    Relay,
    /// A world-facing symbol.
    WorldFacing(OnionServiceName),
}

/// Canonical name of a world-facing symbol: the closed name type `Σ_W` of onion services.
///
/// Invariant: every value denotes a world-facing entry of [`ONION_SIGNATURE`] (see the closure
/// law). It is encoded as that entry's canonical name string.
#[derive(Clone, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionServiceName(&'static OnionSymbolSpec);

impl OnionServiceName {
    /// Parse and canonicalize a service name, admitting exactly the names of `Σ_W`.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed != name {
            return Err(Error::InvalidConfig(
                "onion exit service name must be non-empty and trimmed".to_string(),
            ));
        }
        ONION_SIGNATURE
            .world_facing()
            .find(|service| service.as_str().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "unknown onion service {name:?}; the onion signature is closed: expected one of {}",
                    ONION_SIGNATURE
                        .world_facing()
                        .map(|service| service.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Return the name of the world-facing `https` symbol.
    pub const fn https() -> Self {
        Self(&ONION_SIGNATURE.https)
    }

    /// Return the name of the world-facing `tcp` symbol.
    pub const fn tcp() -> Self {
        Self(&ONION_SIGNATURE.tcp)
    }

    /// Return the canonical name as a string slice.
    pub const fn as_str(&self) -> &'static str {
        self.0.name
    }

    /// Return the specification of the named symbol; total by the closure law.
    pub const fn spec(&self) -> &'static OnionSymbolSpec {
        self.0
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
    use super::OnionSymbol;
    use super::OnionSymbolPosition;
    use super::ONION_SIGNATURE;

    /// Every world-facing name parses to its own entry, case-insensitively, and nothing else
    /// parses: neither names outside `Σ` nor the identity symbol `relay`.
    #[test]
    fn test_service_names_are_exactly_the_world_facing_signature() {
        for service in ONION_SIGNATURE.world_facing() {
            assert_eq!(service.spec().position(), OnionSymbolPosition::WorldFacing);
            assert_eq!(
                OnionServiceName::parse(service.as_str()).ok(),
                Some(service.clone())
            );
            assert_eq!(
                OnionServiceName::parse(service.as_str().to_ascii_uppercase()).ok(),
                Some(service)
            );
        }
        for outside in ["relay", "web", "api", "custom", "tcp2", "", " tcp", "tcp!"] {
            assert!(OnionServiceName::parse(outside).is_err());
        }
    }

    /// Decoding admits only names of `Σ_W`, and the encoding is the bare canonical name string.
    #[test]
    fn test_codec_is_the_name_string_and_rejects_names_outside_the_world_facing_signature() {
        let encoded = rings_codec::serialize(&OnionServiceName::https()).expect("encode name");

        assert_eq!(
            encoded,
            rings_codec::serialize(&"https").expect("encode string")
        );
        for outside in ["web", "relay"] {
            let bytes = rings_codec::serialize(&outside).expect("encode string");
            assert!(rings_codec::deserialize::<OnionServiceName>(bytes.as_slice()).is_err());
        }
    }

    /// `relay` is the only intermediate symbol of `Σ`.
    #[test]
    fn test_relay_is_the_only_intermediate_symbol() {
        let intermediate = ONION_SIGNATURE
            .symbols()
            .into_iter()
            .filter(|spec| spec.position() == OnionSymbolPosition::Intermediate)
            .collect::<Vec<_>>();

        assert_eq!(intermediate, vec![ONION_SIGNATURE.relay()]);
    }

    /// `by_code ∘ code = Some` on `Σ`, codes are the table positions, and no code past the table
    /// names a symbol.
    #[test]
    fn test_by_code_inverts_code() {
        let symbols = ONION_SIGNATURE.symbols();
        for (position, spec) in symbols.into_iter().enumerate() {
            let code = ONION_SIGNATURE.code(spec);
            let expected = if spec == ONION_SIGNATURE.relay() {
                OnionSymbol::Relay
            } else {
                OnionSymbol::WorldFacing(OnionServiceName::parse(spec.name()).expect("Σ_W"))
            };

            assert_eq!(usize::from(code), position);
            assert_eq!(ONION_SIGNATURE.by_code(code), Some(expected));
        }
        let past = u8::try_from(symbols.len()).expect("Σ fits a byte");
        assert_eq!(ONION_SIGNATURE.by_code(past), None);
    }
}
