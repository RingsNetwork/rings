//! The quantised expiry `x` of an onion loop: a point of the grid `Q·ℕ`.
//!
//! The grid is `x = ⌈t_build / Q⌉·Q + X₀` with `X₀ = ONION_FORWARD_PAYLOAD_TTL_MS`, a whole number of
//! quanta. The type holds the quantum index `x / Q`, and its field is private to this module, so an
//! [`OnionExpiry`] comes only from a build instant ([`OnionExpiry::of_build`]) or from parsing an
//! on-grid wire value ([`OnionExpiry::from_ms`]).

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
pub(super) struct OnionExpiry(u128);

impl OnionExpiry {
    /// The expiry of a loop built at `built_at_ms`: `x = ⌈t_build / Q⌉·Q + X₀`. Every build instant in
    /// one quantum maps to the same `x`, so a layer does not reveal the client's clock at a finer
    /// resolution than `Q`.
    pub(super) fn of_build(built_at_ms: u128) -> Self {
        Self(
            built_at_ms
                .div_ceil(ONION_FORWARD_EXPIRY_QUANTUM_MS)
                .saturating_add(ONION_FORWARD_PAYLOAD_TTL_MS / ONION_FORWARD_EXPIRY_QUANTUM_MS),
        )
    }

    /// Parse a wire expiry: `Some` iff `ms` lies on the grid `Q·ℕ`. This is the shell's only way from
    /// a layer's raw `expires_at_ms` to an [`OnionExpiry`].
    #[cfg_attr(
        not(all(test, rings_native)),
        expect(
            dead_code,
            reason = "the shell parses layer expiries with it in #834 Phase 2a-4 (#843)"
        )
    )]
    pub(super) fn from_ms(ms: u128) -> Option<Self> {
        ms.is_multiple_of(ONION_FORWARD_EXPIRY_QUANTUM_MS)
            .then_some(Self(ms / ONION_FORWARD_EXPIRY_QUANTUM_MS))
    }

    /// The expiry instant in milliseconds, `x = k·Q`.
    ///
    /// It saturates at `u128::MAX` only for a quantum index beyond `u128::MAX / Q`, which no clock
    /// in milliseconds since 1970 reaches. For every reachable expiry, `from_ms(as_ms(x)) = Some(x)`.
    pub(super) const fn as_ms(self) -> u128 {
        self.0.saturating_mul(ONION_FORWARD_EXPIRY_QUANTUM_MS)
    }
}
