//! Law tests of the Sphinx primitives (#840), one module per law of the parent documentation.
//!
//! Every test is deterministic: randomness comes from seeded [`StdRng`]s and hop keys from fixed
//! scalars, so a failure reproduces bit for bit.

mod test_carry;
mod test_cell;
mod test_header;
mod test_layer;
mod test_peel_cost;
mod test_seed;

use rand::rngs::StdRng;
use rand::Rng;
use rand::RngCore;
use rand::SeedableRng;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;

use super::header::OnionHeaderHop;
use super::header::OnionHeaderRoute;
use super::layer::OnionArguments;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::layer::OnionLayerHead;
use super::layer::ONION_ARGUMENT_BYTES;
use super::seed::OnionCarrySeed;
use super::seed::OnionSegmentSeed;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionServiceName;

/// The fixture RNG, seeded per test so that tests are independent of their order.
fn fixture_rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

/// The delegatee key of hop `index`: the fixed scalar `(index + 1)·0x0101…01`.
fn hop_key(index: usize) -> DelegateeKey {
    let byte = u8::try_from(index).expect("fixture index fits a byte") + 1;
    let secret =
        SecretKey::try_from(format!("{byte:02x}").repeat(32).as_str()).expect("fixture scalar");
    DelegateeKey::new_with_seckey(&secret).expect("fixture delegation")
}

/// The layer of `position` in a loop of `hops` positions under fixture `seed`: `relay` everywhere
/// but the last position, which applies `tcp`; every other field uniform. A function of its
/// arguments, so a test rebuilds the expected layer instead of cloning key material.
fn fixture_layer(seed: u64, position: usize, hops: usize) -> OnionLayer {
    let offset = u64::try_from(position).expect("fixture position fits u64");
    let mut rng = fixture_rng(seed.wrapping_mul(1 << 16).wrapping_add(offset));
    let application = if position + 1 == hops {
        let mut arguments = [0; ONION_ARGUMENT_BYTES];
        rng.fill_bytes(&mut arguments);
        OnionLayerApplication::Apply {
            symbol: OnionServiceName::tcp(),
            arguments: OnionArguments::new(arguments),
        }
    } else {
        OnionLayerApplication::Relay
    };
    OnionLayer {
        head: OnionLayerHead {
            application,
            next: Did::from(u32::try_from(position).expect("fixture position fits u32")),
            epoch: OnionExitEpoch::new(rng.gen()),
            expires_at_ms: rng.gen(),
            nonce: OnionForwardNonce::new(rng.gen()),
        },
        inbound: OnionCarrySeed::new(rng.gen()),
        outbound: OnionSegmentSeed::random(&mut rng),
    }
}

/// The route of a loop of `hops` positions whose hop at each position holds `keys[position]`.
fn fixture_route(seed: u64, keys: &[DelegateeKey]) -> OnionHeaderRoute {
    OnionHeaderRoute::new(
        keys.iter()
            .enumerate()
            .map(|(position, key)| OnionHeaderHop {
                public_key: key.delegatee_public_key(),
                layer: fixture_layer(seed, position, keys.len()),
            })
            .collect(),
    )
    .expect("fixture loop length")
}

/// The keys of a loop of `hops` distinct hops.
fn fixture_keys(hops: usize) -> Vec<DelegateeKey> {
    (0..hops).map(hop_key).collect()
}
