//! Law tests of the Sphinx primitives (#840), one module per law of the parent documentation.
//!
//! Every test is deterministic: randomness comes from seeded [`StdRng`]s and hop keys from fixed
//! scalars, so a failure reproduces bit for bit.

mod test_builder;
mod test_carry;
mod test_cell;
mod test_header;
mod test_layer;
mod test_peel_cost;
mod test_seed;

use std::num::NonZeroUsize;

use rand::rngs::StdRng;
use rand::Rng;
use rand::RngCore;
use rand::SeedableRng;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::swarm::callback::PeerLink;

use super::cell::Charged;
use super::cell::OnionAdmittedCell;
use super::cell::OnionCell;
use super::header::OnionHeaderHop;
use super::header::OnionHeaderRoute;
use super::layer::OnionArguments;
use super::layer::OnionLayer;
use super::layer::OnionLayerApplication;
use super::layer::OnionLayerHead;
use super::layer::ONION_ARGUMENT_BYTES;
use super::seed::OnionCarrySeed;
use super::seed::OnionSegmentSeed;
use crate::onion::circuit::OnionAdmissionState;
use crate::onion::circuit::OnionExpiry;
use crate::onion::circuit::OnionReplayFilterKey;
use crate::onion::circuit::OnionReplayNonce;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionServiceName;

/// The process epoch every fixture hop runs and every fixture layer is sealed for.
const FIXTURE_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([1; 16]);

/// The arrival instant of every fixture cell: `2Q`, so the admission window `(2Q, 7Q]` holds the
/// fixture expiries `3Q … 7Q`.
const FIXTURE_ARRIVAL_MS: u128 = 60_000;

/// The one link every fixture cell arrives on.
fn fixture_link() -> PeerLink {
    PeerLink::new(Did::from(1_u32), 0)
}

/// The expiry `x = (3 + offset)·Q` of a fixture layer, admissible at [`FIXTURE_ARRIVAL_MS`] for
/// `offset < 5`.
fn fixture_expiry(offset: u32) -> OnionExpiry {
    OnionExpiry::from_ms(u128::from(3 + offset) * 30_000).expect("on the grid")
}

/// A fresh hop admission at [`FIXTURE_EPOCH`] with [`fixture_link`] live.
fn fixture_admission() -> OnionAdmissionState {
    let mut admission = OnionAdmissionState::new(
        FIXTURE_EPOCH,
        OnionReplayFilterKey::new([7; 32]),
        NonZeroUsize::MIN,
    );
    admission
        .link_opened(FIXTURE_ARRIVAL_MS, fixture_link())
        .expect("an empty table admits a link");
    admission
}

/// The bytes charged as a cell at a fresh fixture hop, as a hop receives them.
fn charged(bytes: Vec<u8>) -> Charged<OnionCell> {
    fixture_admission()
        .charge(
            FIXTURE_ARRIVAL_MS,
            fixture_link(),
            OnionCell::parse(&bytes).expect("a cell"),
        )
        .expect("the fixture link has budget")
}

/// The whole paid path of one hop, charge ∘ peel ∘ admit, under `key`, at a fresh fixture hop.
fn admitted(bytes: Vec<u8>, key: &DelegateeKey) -> OnionAdmittedCell {
    let mut admission = fixture_admission();
    admission
        .charge(
            FIXTURE_ARRIVAL_MS,
            fixture_link(),
            OnionCell::parse(&bytes).expect("a cell"),
        )
        .expect("the fixture link has budget")
        .peel(key)
        .expect("peel")
        .admit(&mut admission, FIXTURE_ARRIVAL_MS)
        .expect("a fixture layer is admissible")
}

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
            epoch: FIXTURE_EPOCH,
            expiry: fixture_expiry(0),
            nonce: OnionReplayNonce::new(rng.gen()),
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
