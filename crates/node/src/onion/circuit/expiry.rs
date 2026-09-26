//! The quantised expiry `x` of an onion loop: a point of the grid `Q·ℕ`.
//!
//! The grid is `x = ⌈t_build / Q⌉·Q + X₀` with `X₀ = ONION_FORWARD_PAYLOAD_TTL_MS`, a whole number
//! of quanta. The type holds the quantum index `x / Q`, and its field is private to this module, so
//! an [`OnionExpiry`] comes only from a build instant ([`OnionExpiry::of_build`]) or from parsing
//! an on-grid wire value ([`OnionExpiry::from_ms`]).

use super::ONION_FORWARD_EXPIRY_QUANTUM_MS;
use super::ONION_FORWARD_MAX_VALIDITY_MS;
use super::ONION_FORWARD_PAYLOAD_TTL_MS;

/// Compile-time law: `X₀` and `V` are whole numbers of quanta, so [`OnionExpiry::of_build`]
/// stays on the grid.
const _: () = assert!(
    ONION_FORWARD_PAYLOAD_TTL_MS.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
        && ONION_FORWARD_MAX_VALIDITY_MS.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
);

/// A layer's quantised expiry `x ∈ Q·ℕ`, held as its quantum index `x / Q`, so an off-grid
/// instant is unrepresentable.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionExpiry(u128);

impl OnionExpiry {
    /// The expiry of a loop built at `built_at_ms`: `x = ⌈t_build / Q⌉·Q + X₀`. Every build instant
    /// in one quantum maps to the same `x`, so a layer does not reveal the client's clock at a
    /// finer resolution than `Q`.
    pub(crate) fn of_build(built_at_ms: u128) -> Self {
        Self(
            built_at_ms
                .div_ceil(ONION_FORWARD_EXPIRY_QUANTUM_MS)
                .saturating_add(ONION_FORWARD_PAYLOAD_TTL_MS / ONION_FORWARD_EXPIRY_QUANTUM_MS),
        )
    }

    /// An expiry of `ms` milliseconds: `Some` iff `ms` lies on the grid `Q·ℕ`. The wire decoders
    /// use [`Self::from_wire_ms`]; this is the constructor of a time already in milliseconds.
    pub(crate) fn from_ms(ms: u128) -> Option<Self> {
        ms.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
            .then_some(Self(ms / ONION_FORWARD_EXPIRY_QUANTUM_MS))
    }

    /// The admission window (#834 L9): `arr < x ≤ arr + V`, the only instants at which a layer
    /// expiring at `x` may be admitted by any hop.
    pub(crate) const fn admissible_at(self, arrival_ms: u128) -> bool {
        let expiry_ms = self.as_ms();
        arrival_ms < expiry_ms
            && expiry_ms <= arrival_ms.saturating_add(ONION_FORWARD_MAX_VALIDITY_MS)
    }

    /// Whether `x` has passed at `now`, i.e. `x ≤ now`: the replay filter of `x` is gone, and no
    /// layer expiring at `x` may be admitted any more.
    pub(crate) const fn has_passed_at(self, now_ms: u128) -> bool {
        self.as_ms() <= now_ms
    }

    /// The expiry instant in milliseconds, `x = k·Q`.
    ///
    /// It saturates at `u128::MAX` only for a quantum index beyond `u128::MAX / Q`, which no clock
    /// in milliseconds since 1970 reaches. For every reachable expiry, `from_ms(as_ms(x)) =
    /// Some(x)`.
    pub(crate) const fn as_ms(self) -> u128 {
        self.0.saturating_mul(ONION_FORWARD_EXPIRY_QUANTUM_MS)
    }

    /// The expiry as the 64-bit field of a layer or reply block: `x` in milliseconds.
    ///
    /// An expiry past `u64::MAX` ms, which no clock reaches, encodes as `u64::MAX`, which is off
    /// the grid, so [`Self::from_wire_ms`] rejects it and no hop admits the layer.
    pub(crate) fn to_wire_ms(self) -> u64 {
        u64::try_from(self.as_ms()).unwrap_or(u64::MAX)
    }

    /// Parse the 64-bit wire field: `Some` iff it lies on the grid. The left inverse of
    /// [`Self::to_wire_ms`] on every reachable expiry.
    pub(crate) fn from_wire_ms(ms: u64) -> Option<Self> {
        Self::from_ms(u128::from(ms))
    }
}
