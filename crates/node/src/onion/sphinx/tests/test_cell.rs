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
use crate::onion::sphinx::cell::OnionProduceError;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeaderHop;
use crate::onion::sphinx::header::OnionHeaderRoute;
use crate::onion::sphinx::layer::OnionArguments;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerApplication;
use crate::onion::sphinx::layer::OnionLayerHead;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionServiceName;

/// The layer of `position` with the given application and carry seeds; its `next` and `x` are
/// distinct per position (`100 + position`, `1000 + position`), so the tests can tell which layer
/// a value came from.
fn layer(
    position: u32,
    application: OnionLayerApplication,
    inbound: OnionCarrySeed,
    outbound: OnionSegmentSeed,
) -> OnionLayer {
    OnionLayer {
        head: OnionLayerHead {
            application,
            next: Did::from(100 + position),
            epoch: OnionProcessEpoch::new([1; 16]),
            expires_at_ms: 1000 + u64::from(position),
            nonce: OnionForwardNonce::new([2; 16]),
        },
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
            0,
            OnionLayerApplication::Relay,
            inbound_1,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            1,
            OnionLayerApplication::Relay,
            inbound_2,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            2,
            OnionLayerApplication::Apply {
                symbol: OnionServiceName::tcp(),
                arguments: OnionArguments::new([3; 64]),
            },
            first.seeds().consumer,
            OnionSegmentSeed::new(*second.as_bytes()),
        ),
        layer(
            3,
            OnionLayerApplication::Relay,
            inbound_4,
            OnionSegmentSeed::random(&mut rng),
        ),
        layer(
            4,
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
    let peel = |cell: Vec<u8>, position: usize| {
        let peeled = OnionCell::parse(cell)
            .expect("cell")
            .peel(&keys[position])
            .expect("peel");
        // Admission reads this position's own layer before any carry work.
        let index = u64::try_from(position).expect("small");
        assert_eq!(peeled.head().expires_at_ms, 1000 + index);
        peeled.step().expect("carry step")
    };
    let relay = |cell: Vec<u8>, position| {
        let OnionStep::Relayed { head, cell } = peel(cell, position) else {
            panic!("a relay position");
        };
        assert_eq!(head.application, OnionLayerApplication::Relay);
        assert_eq!(cell.class(), class);
        cell.into_bytes()
    };

    let cell = relay(relay(cell.into_bytes(), 0), 1);
    let OnionStep::Consumed { head, value, surb } = peel(cell, 2) else {
        panic!("the symbol position");
    };
    assert!(matches!(
        head.application,
        OnionLayerApplication::Apply { .. }
    ));
    assert_eq!(*value, input);
    // υ carries the symbol layer's own `next` and `x` (position 2), so a pool needs nothing
    // beside it; a value too wide for the class hands the block back unspent.
    assert_eq!(surb.expires_at_ms(), 1002);
    let overwide = vec![0; surb.capacity() + 1];
    let Err(OnionProduceError::ValueTooWide { surb, .. }) = surb.produce(&overwide) else {
        panic!("a value one byte over capacity");
    };
    let (next, cell) = surb.produce(&output).expect("produce");
    assert_eq!(next, Did::from(102_u32));
    let cell = relay(relay(cell.into_bytes(), 3), 4);
    let returned = OnionCell::parse(cell).expect("cell");

    assert_eq!(returned.loop_tag(), tag);
    assert_eq!(
        *returned
            .open(&second.seeds().consumer.key().expect("strong key"))
            .expect("the client receives v₁"),
        output
    );
}
