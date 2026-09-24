//! One whole loop at the cell level (D4, D5, D6′, D7): the client, `s = 2` relays, a symbol hop,
//! `s = 2` relays with the guard last, and the client again, every cell parsed from its bytes.
//!
//! ```text
//! client ─seal σ₀─▶ r₀₁ ─▶ r₀₂ ─▶ h₁ (consume v₀, produce v₁ under σ₁) ─▶ r₁₁ ─▶ r₁₂ = g ─▶ client
//! ```
//!
//! Laws: every relay is the identity on the carried value (L1), the consumer of each segment
//! receives exactly its producer's value (D7), the class is the one the client chose on every
//! edge, and the client receives its tag `t_⋄` (D6′).

use rand::Rng;
use rings_core::dht::Did;
use subtle::ConstantTimeEq;

use super::fixture_keys;
use super::fixture_rng;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::header::OnionHeaderHop;
use crate::onion::sphinx::header::OnionHeaderRoute;
use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;
use crate::onion::OnionExitEpoch;
use crate::onion::OnionServiceName;

/// A layer with the given application and carry seeds; its other fields are fixed.
fn layer(
    application: OnionLayerApplication,
    inbound: OnionCarrySeed,
    outbound: OnionSegmentSeed,
) -> OnionLayer {
    OnionLayer {
        application,
        next: Did::from(7_u32),
        epoch: OnionExitEpoch::new([1; 16]),
        expires_at_ms: 1,
        nonce: OnionForwardNonce::new([2; 16]),
        inbound,
        outbound,
    }
}

/// The client's value `v₀` reaches `h₁`, whose output `v₁` reaches the client, through relays
/// that only re-encipher, in one class on every edge and with the client's tag at the end.
#[test]
fn test_loop_carries_each_segment_value_to_its_consumer() {
    let mut rng = fixture_rng(50);
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(5);
    let (first, first_keys) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let (second, _) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let [inbound_1, inbound_2] = first.seeds().relays;
    let [inbound_4, inbound_5] = second.seeds().relays;
    let layers = [
        layer(
            OnionLayerApplication::Relay,
            inbound_1,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            OnionLayerApplication::Relay,
            inbound_2,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            OnionLayerApplication::Apply {
                symbol: OnionServiceName::tcp(),
                arguments: OnionArguments::new([3; 64]),
            },
            first.seeds().consumer,
            OnionSegmentSeed::new(*second.as_bytes()),
        ),
        layer(
            OnionLayerApplication::Relay,
            inbound_4,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            OnionLayerApplication::Relay,
            inbound_5,
            OnionSegmentSeed::random(&mut rng),
        ),
    ];
    let route = OnionHeaderRoute::new(
        keys.iter()
            .zip(layers)
            .map(|(key, layer)| OnionHeaderHop {
                public_key: key.delegatee_public_key(),
                layer,
            })
            .collect(),
    )
    .expect("loop length");
    let (header, tag) = OnionHeader::build(&route, class, &mut rng).expect("build the header");
    let input = rng.gen::<[u8; 32]>();
    let output = rng.gen::<[u8; 24]>();
    let relay = |cell: Vec<u8>, key| {
        let peeled = OnionCell::parse(&cell)
            .expect("cell")
            .peel(key)
            .expect("peel");
        assert_eq!(peeled.layer().application, OnionLayerApplication::Relay);
        let (_, forwarded) = peeled.relay().expect("strong key");
        assert_eq!(forwarded.class(), class);
        forwarded.to_bytes()
    };

    let cell = OnionCell::seal(class, header, &first_keys, &input)
        .expect("seal")
        .to_bytes();
    let cell = relay(relay(cell, &keys[0]), &keys[1]);
    let (layer, value, producer) = OnionCell::parse(&cell)
        .expect("cell")
        .peel(&keys[2])
        .expect("peel")
        .consume()
        .expect("h₁ receives v₀");
    assert_eq!(value, input);
    let cell = producer
        .produce(&layer.outbound.keys().expect("strong segment"), &output)
        .expect("produce")
        .to_bytes();
    let cell = relay(relay(cell, &keys[3]), &keys[4]);
    let returned = OnionCell::parse(&cell).expect("cell");

    assert!(bool::from(returned.loop_tag().ct_eq(&tag)));
    assert_eq!(
        returned
            .open(&second.seeds().consumer.key().expect("strong key"))
            .expect("the client receives v₁"),
        output
    );
}
