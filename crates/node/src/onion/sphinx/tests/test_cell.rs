//! One whole loop at the cell level (D4, D5, D6′, D7): the client, `s = 2` relays, a symbol hop,
//! `s = 2` relays with the guard last, and the client again, every cell parsed from its bytes.
//!
//! ```text
//! client ─σ₀─▶ g = r₀₁ ─▶ r₀₂ ─▶ h₁ (consume v₀, produce v₁ under σ₁) ─▶ r₁₁ ─▶ r₁₂ = g ─▶ client
//! ```
//!
//! Laws: every relay is the identity on the carried value (L1), the consumer of each segment
//! receives exactly its producer's value (D7), the class is the one the client chose on every
//! edge, the guard peels both of its positions with one key (L7), and the client receives its tag
//! `t_⋄` (D6′).

use rand::Rng;
use rings_core::dht::Did;

use super::fixture_keys;
use super::fixture_rng;
use super::hop_key;
use crate::onion::circuit::OnionForwardNonce;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::class::OnionLoopClass;
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
    let mut keys = fixture_keys(4);
    keys.push(hop_key(0));
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
    let input = rng.gen::<[u8; 32]>();
    let output = rng.gen::<[u8; 24]>();
    let (cell, tag) = OnionCell::client(&route, class, &first_keys, &input, &mut rng)
        .expect("the client's first cell");
    let peel = |cell: Vec<u8>, key| {
        let peeled = OnionCell::parse(cell)
            .expect("cell")
            .peel(key)
            .expect("peel");
        // Admission reads the layer before any carry work.
        assert_eq!(peeled.layer().expires_at_ms, 1);
        peeled.step().expect("carry step")
    };
    let relay = |cell: Vec<u8>, key| {
        let OnionStep::Relayed { layer, cell } = peel(cell, key) else {
            panic!("a relay position");
        };
        assert_eq!(layer.application, OnionLayerApplication::Relay);
        assert_eq!(cell.class(), class);
        cell.into_bytes()
    };

    let cell = relay(relay(cell.into_bytes(), &keys[0]), &keys[1]);
    let OnionStep::Consumed { layer, value, surb } = peel(cell, &keys[2]) else {
        panic!("the symbol position");
    };
    assert!(matches!(
        layer.application,
        OnionLayerApplication::Apply { .. }
    ));
    assert_eq!(*value, input);
    let cell = surb.produce(&output).expect("produce").into_bytes();
    let cell = relay(relay(cell, &keys[3]), &keys[4]);
    let returned = OnionCell::parse(cell).expect("cell");

    assert_eq!(returned.loop_tag(), tag);
    assert_eq!(
        *returned
            .open(&second.seeds().consumer.key().expect("strong key"))
            .expect("the client receives v₁"),
        output
    );
}
