//! Law tests of the Sphinx primitives (#840), one module per law of the parent documentation.
//!
//! Every test is deterministic: randomness comes from a seeded [`StdRng`] and hop keys from fixed
//! scalars, so a failure reproduces bit for bit.

mod test_carry;
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
use super::layer::OnionArguments;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::layer::OnionLoopTag;
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

/// The delegatee key of the hop at `position`: the fixed scalar `(position + 1)·0x0101…01`.
fn hop_key(position: usize) -> DelegateeKey {
    let byte = u8::try_from(position).expect("fixture position fits a byte") + 1;
    let secret =
        SecretKey::try_from(format!("{byte:02x}").repeat(32).as_str()).expect("fixture scalar");
    DelegateeKey::new_with_seckey(&secret).expect("fixture delegation")
}

/// A layer for `position` of a loop of `hops` positions: `relay` everywhere but the last
/// position, which applies `tcp`; every other field uniform.
fn fixture_layer(rng: &mut StdRng, position: usize, hops: usize) -> OnionLayer {
    let application = if position.saturating_add(1) == hops {
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
        application,
        next: Did::from(u32::try_from(position).expect("fixture position fits u32")),
        epoch: OnionExitEpoch::new(rng.gen()),
        expires_at_ms: rng.gen(),
        nonce: OnionForwardNonce::new(rng.gen()),
        inbound: OnionCarrySeed::new(rng.gen()),
        outbound: OnionSegmentSeed::random(rng),
        tag: OnionLoopTag::new(rng.gen()),
    }
}

/// The keys and layers of a loop of `hops` positions, and the hops the client builds from.
fn fixture_loop(
    rng: &mut StdRng,
    hops: usize,
) -> (Vec<DelegateeKey>, Vec<OnionLayer>, Vec<OnionHeaderHop>) {
    let keys = (0..hops).map(hop_key).collect::<Vec<_>>();
    let layers = (0..hops)
        .map(|position| fixture_layer(rng, position, hops))
        .collect::<Vec<_>>();
    let route = keys
        .iter()
        .zip(layers.iter())
        .map(|(key, layer)| OnionHeaderHop {
            public_key: key.delegatee_public_key(),
            layer: layer.clone(),
        })
        .collect();
    (keys, layers, route)
}
