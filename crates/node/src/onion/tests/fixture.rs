//! Fixtures shared by the onion module's law tests: fixed scalars, route hops, the session loop
//! `g, r₀₂, h, r₁₁, g` and grid expiries. Every value is fixed, so a failure reproduces bit for
//! bit.

use rings_core::delegation::DelegateeKey;
use rings_core::ecc::SecretKey;

use crate::error::Error;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::ONION_EXPIRY_QUANTUM_MS;
use crate::onion::OnionLoop;
use crate::onion::OnionLoopRelay;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteHop;

/// The fixed scalar `(seed)·0x0101…01`.
pub(crate) fn fixture_secret(seed: u8) -> SecretKey {
    SecretKey::try_from(format!("{seed:02x}").repeat(32).as_str()).expect("fixture scalar")
}

/// The route hop of the session key `key`, registered at `epoch`.
pub(crate) fn fixture_route_hop(key: &DelegateeKey, epoch: OnionProcessEpoch) -> OnionRouteHop {
    OnionRouteHop::new(key.delegator_did(), key.delegatee_public_key(), epoch)
}

/// The session loop `g, r₀₂, h, r₁₁, g` over the hops `[g, r₀₂, h, r₁₁]`: `h` is the symbol hop,
/// and the unfold asks for the guard first, then `r₀₂`, then `r₁₁`.
pub(crate) fn session_loop(hops: [OnionRouteHop; 4]) -> OnionLoop<OnionRouteHop> {
    let [g, r02, h, r11] = hops;
    let mut relays = [(g, true), (r02, false), (r11, false)].into_iter();
    OnionLoop::try_unfold(Vec::new(), h, |relay| {
        let (hop, guard) = relays.next().ok_or(Error::InvalidData)?;
        assert_eq!(relay == OnionLoopRelay::Guard, guard);
        Ok(hop)
    })
    .expect("the session loop")
}

/// The grid expiry `x = quanta·Q`.
pub(crate) fn grid_expiry(quanta: u128) -> OnionExpiry {
    OnionExpiry::from_ms(quanta * ONION_EXPIRY_QUANTUM_MS).expect("on the grid")
}
