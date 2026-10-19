//! The onion signature `Σ`: the static table of operation symbols a loop applies.
//!
//! A loop evaluates a term over `Σ`, one symbol per hop (#834 D1). This phase fixes
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
//! - **Identity** (#834 L1). `relay = id`, so `relay ⋙ f = f` on carried values. The pure hop step
//!   interprets `relay`; no exit adapter ever does, and `relay` is registered through the
//!   online-node relay capability, never as a service (#834 D2).
//! - **Position.** A world-facing symbol exchanges bytes with the outside world and stands last.
//! - **Closure.** `Σ` is closed and [`OnionServiceName`] is exactly `Σ_W`: `OnionServiceName ≅ Σ_W`.
//!   Parsing is the only way in, so a name outside `Σ_W`, `relay` included, is rejected wherever it
//!   enters the node (configuration, descriptor decode, RPC), and no route, exit layer or algebra
//!   entry can name it. The encoding is the canonical name string, so closure changes no wire byte.
//! - **One interpretation per symbol** (#834 D1′). `https` is a request/response fetch only, and
//!   every byte tunnel, TLS included, is `tcp`. No interpretation falls back to another symbol, so
//!   a browser exit, which has `fetch` but no sockets, interprets `https` alone, and HTTPS-only
//!   egress is `tcp` under a `*:443` policy.
//!
//! - **Code.** `Σ = {relay} ⊎ Σ_W` is the coproduct `OnionSymbol = 1 + Σ_W`, and the `f` byte
//!   of the uniform layer (#834 D6″) is its position in table order: `code(inl) = 0`,
//!   `code(inr w) = 1 + w`, with `from_code ∘ code = Some` and `from_code` undefined past `Σ`.
//!   All of it is derived from one table (`onion_signature!`).
//!
//! Phase 2b note: the `invoke` shapes of #834 D1′ are intermediate but not `relay`, so there the
//! coproduct becomes `1 + Σ_I + Σ_W` and the code orders `relay`, then `Σ_I`, then `Σ_W`. That is
//! a change to this table and its codec at the cutover, not to their consumers; the statements
//! "`Σ = {relay} ⊎ Σ_W`" and "`relay` is the only intermediate symbol" hold for Phases 1 and 2a.
//!
//! Width and latency classes `W`, `L` of the world-facing symbols are not protocol data yet: a
//! world-facing symbol is a session whose results return in reply blocks the client supplies
//! (#834 D8), each carrying at most the value capacity of the loop's class.

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

/// Declares `Σ` once, in table order: the identity symbol, then one row per world-facing symbol.
/// Every representation of `Σ_W` is derived from the rows, so adding a symbol is one row and no
/// other place can fall out of step:
///
/// ```text
/// row  Variant field "name"  ↦  OnionSignature.field and its entry in ONION_SIGNATURE,
///                                its place in symbols() (table order = layer code),
///                                OnionWorldSymbol::Variant, its place in OnionWorldSymbol::ALL,
///                                spec(Variant) = &ONION_SIGNATURE.field,
///                                OnionServiceName::field()
/// ```
macro_rules! onion_signature {
    (
        relay: $relay:literal,
        world_facing: [
            $($(#[doc = $doc:literal])* $variant:ident $field:ident $name:literal,)+
        ] $(,)?
    ) => {
        /// The finite signature `Σ`, one named field per symbol so that every table access is
        /// total.
        #[derive(Debug)]
        pub struct OnionSignature {
            /// `relay`.
            relay: OnionSymbolSpec,
            $($(#[doc = $doc])* $field: OnionSymbolSpec,)+
        }

        /// The onion signature of this node generation.
        pub static ONION_SIGNATURE: OnionSignature = OnionSignature {
            relay: OnionSymbolSpec::new($relay, OnionSymbolPosition::Intermediate),
            $($field: OnionSymbolSpec::new($name, OnionSymbolPosition::WorldFacing),)+
        };

        /// `|Σ_W|`, the number of world-facing rows.
        const WORLD_FACING_SYMBOLS: usize = [$(onion_signature!(@unit $variant),)+].len();

        impl OnionSignature {
            /// Return the identity symbol `relay`.
            pub const fn relay(&self) -> &OnionSymbolSpec {
                &self.relay
            }

            /// Return `Σ` in table order, which is the order of the layer codes.
            pub const fn symbols(&self) -> [&OnionSymbolSpec; 1 + WORLD_FACING_SYMBOLS] {
                [&self.relay, $(&self.$field,)+]
            }

        }

        /// `Σ_W` as a type: one constructor per world-facing row, in table order; the
        /// discriminant is the position within `Σ_W`.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        #[repr(u8)]
        enum OnionWorldSymbol {
            $($(#[doc = $doc])* $variant,)+
        }

        impl OnionWorldSymbol {
            /// `Σ_W` in table order.
            const ALL: [Self; WORLD_FACING_SYMBOLS] = [$(Self::$variant,)+];

            /// The table entry of this symbol.
            const fn spec(self) -> &'static OnionSymbolSpec {
                match self {
                    $(Self::$variant => &ONION_SIGNATURE.$field,)+
                }
            }
        }

        impl OnionServiceName {
            /// Return `Σ_W`, the world-facing symbols of `Σ`, in table order.
            pub fn world_facing() -> impl Iterator<Item = Self> {
                OnionWorldSymbol::ALL.into_iter().map(Self)
            }

            $(
                #[doc = concat!("Return the name of the world-facing `", $name, "` symbol.")]
                pub const fn $field() -> Self {
                    Self(OnionWorldSymbol::$variant)
                }
            )+
        }
    };
    (@unit $variant:ident) => {
        ()
    };
}

onion_signature! {
    relay: "relay",
    world_facing: [
        /// `tcp`, a world-facing byte stream.
        Tcp tcp "tcp",
        /// `https`, a world-facing request/response (fetch).
        Https https "https",
    ],
}

/// A symbol of `Σ = {relay} ⊎ Σ_W`, as the coproduct `1 + Σ_W`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OnionSymbol {
    /// The identity symbol `relay`, the only intermediate symbol.
    Relay,
    /// A world-facing symbol.
    WorldFacing(OnionServiceName),
}

impl OnionSymbol {
    /// `code : 1 + Σ_W → u8`, `inl ↦ 0`, `inr w ↦ 1 + w`: the position in table order.
    pub(crate) const fn code(&self) -> u8 {
        match self {
            Self::Relay => 0,
            // A fieldless `repr(u8)` discriminant: its position within `Σ_W`.
            Self::WorldFacing(OnionServiceName(symbol)) => 1 + *symbol as u8,
        }
    }

    /// The left inverse of [`Self::code`]; `None` past the end of `Σ`.
    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code.checked_sub(1) {
            None => Some(Self::Relay),
            Some(world) => OnionWorldSymbol::ALL
                .into_iter()
                .nth(usize::from(world))
                .map(|symbol| Self::WorldFacing(OnionServiceName(symbol))),
        }
    }
}

/// Canonical name of a world-facing symbol: the closed name type `Σ_W` of onion services.
///
/// Invariant: every value denotes a world-facing entry of [`ONION_SIGNATURE`] (see the closure
/// law). It is encoded as that entry's canonical name string.
#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionServiceName(OnionWorldSymbol);

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
        OnionServiceName::world_facing()
            .find(|service| service.as_str().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                Error::InvalidConfig(format!(
                    "unknown onion service {name:?}; the onion signature is closed: expected one of {}",
                    OnionServiceName::world_facing()
                        .map(|service| service.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    /// Return the canonical name as a string slice.
    pub const fn as_str(&self) -> &'static str {
        self.0.spec().name
    }

    /// Return the specification of the named symbol; total by the closure law.
    pub const fn spec(&self) -> &'static OnionSymbolSpec {
        self.0.spec()
    }

    /// Return whether this name equals `service` after service-name canonicalization.
    pub fn matches(&self, service: &str) -> bool {
        Self::parse(service).is_ok_and(|candidate| candidate == *self)
    }
}

impl fmt::Debug for OnionServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnionServiceName")
            .field(&self.as_str())
            .finish()
    }
}

impl PartialOrd for OnionServiceName {
    /// Names order by their canonical string, so `Σ_W` sorts the same wherever it is keyed.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OnionServiceName {
    /// Names order by their canonical string.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
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
        for service in OnionServiceName::world_facing() {
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

    /// `from_code ∘ code = Some` on `Σ`, every code is its symbol's position in `symbols()`, every
    /// symbol has a code, and no code past the table names a symbol. Enumerated over all of
    /// `u8`, so no hand-written list can hide a missing row.
    #[test]
    fn test_from_code_inverts_code() {
        let decoded = (0..=u8::MAX)
            .filter_map(|code| OnionSymbol::from_code(code).map(|symbol| (code, symbol)))
            .collect::<Vec<_>>();

        assert_eq!(decoded.len(), ONION_SIGNATURE.symbols().len());
        for ((code, symbol), (position, spec)) in decoded
            .iter()
            .zip(ONION_SIGNATURE.symbols().into_iter().enumerate())
        {
            let spec_of_symbol = match symbol {
                OnionSymbol::Relay => ONION_SIGNATURE.relay(),
                OnionSymbol::WorldFacing(name) => name.spec(),
            };

            assert_eq!(usize::from(*code), position);
            assert_eq!(symbol.code(), *code);
            assert_eq!(spec_of_symbol, spec);
        }
        assert_eq!(
            OnionServiceName::world_facing().count() + 1,
            ONION_SIGNATURE.symbols().len()
        );
    }

    /// Service names order by their canonical string.
    #[test]
    fn test_service_names_order_by_name() {
        assert!(OnionServiceName::https() < OnionServiceName::tcp());
    }
}
