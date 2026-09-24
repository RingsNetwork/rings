//! Carry segments (D7): round trip through the keys each hop derives from its own seed, the
//! width law (L3, L5′), and L8 on the carry.

use super::fixture_rng;
use crate::onion::sphinx::carry;
use crate::onion::sphinx::carry::OnionCarry;
use crate::onion::sphinx::carry::OnionOpenError;
use crate::onion::sphinx::carry::OnionValueTooWide;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::seed::OnionCarryKey;
use crate::onion::sphinx::seed::OnionSegmentSeed;
use crate::onion::sphinx::ONION_SEGMENT_RELAYS;

/// The keys a segment's relays and consumer derive from the seeds in their own layers.
fn holder_keys(segment: &OnionSegmentSeed) -> (Vec<OnionCarryKey>, OnionCarryKey) {
    let seeds = segment.seeds();
    let relays = seeds
        .relays
        .iter()
        .map(|seed| seed.key().expect("strong relay key"))
        .collect();
    (relays, seeds.consumer.key().expect("strong consumer key"))
}

/// `y_{k,0}, …, y_{k,s}`: the slot on every edge from `carry` through `relays`.
fn edges(carry: OnionCarry, relays: &[OnionCarryKey]) -> Vec<OnionCarry> {
    core::iter::once(carry.clone())
        .chain(relays.iter().scan(carry, |carry, key| {
            carry::peel(key, carry);
            Some(carry.clone())
        }))
        .collect()
}

/// `open ∘ peel_{r_s} ∘ … ∘ peel_{r_1} ∘ seal = Right` for `ε` (the unit value), a short value
/// and the widest value `pad` admits, with the slot width `C_b` on every edge; one byte wider
/// fails closed.
#[test]
fn test_segment_round_trip_at_every_width() {
    let mut rng = fixture_rng(20);
    let class = OnionLoopClass::DEFAULT;
    let (segment, keys) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let (relays, consumer) = holder_keys(&segment);
    let widest = vec![0xa5; class.carry_value_bytes() - 1];

    for value in [Vec::new(), b"kleisli".to_vec(), widest.clone()] {
        let carry = carry::seal(class, &keys, value.as_slice()).expect("seal");
        let slots = edges(carry, relays.as_slice());

        assert_eq!(slots.len(), ONION_SEGMENT_RELAYS + 1);
        assert!(slots
            .iter()
            .all(|edge| edge.as_slice().len() == class.carry_bytes()));
        let last = slots.last().expect("the consumer's edge").clone();
        assert_eq!(*carry::open(&consumer, last).expect("open"), value);
    }

    let overwide = [widest.as_slice(), &[0]].concat();
    assert_eq!(
        carry::seal(class, &keys, overwide.as_slice()).err(),
        Some(OnionValueTooWide {
            length: class.carry_value_bytes(),
            capacity: class.carry_value_bytes() - 1,
        })
    );
}

/// L8: consecutive slots differ on every edge, and one flipped bit on any edge makes the
/// consumer reject the whole value.
#[test]
fn test_one_bit_flip_on_any_edge_rejects_the_whole_value() {
    let mut rng = fixture_rng(21);
    let class = OnionLoopClass::DEFAULT;
    let (segment, keys) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let (relays, consumer) = holder_keys(&segment);
    let carry = carry::seal(class, &keys, b"one value, one key").expect("seal");
    let slots = edges(carry, relays.as_slice());

    assert!(slots
        .windows(2)
        .all(|pair| pair[0].as_slice() != pair[1].as_slice()));
    let width = class.carry_bytes();
    let offsets = (0..width).step_by(width / 61).chain([
        1,
        15,
        16,
        31,
        32,
        width - 17,
        width - 16,
        width - 1,
    ]);
    for (edge, slot) in slots.iter().enumerate() {
        for offset in offsets.clone() {
            let mut bytes = slot.as_slice().to_vec();
            bytes[offset] ^= 1 << (offset % 8);
            let tampered = OnionCarry::new(bytes).expect("same width");

            let rejection = edges(tampered, &relays[edge..])
                .pop()
                .map(|slot| carry::open(&consumer, slot).err())
                .expect("the consumer's edge");

            assert_eq!(
                rejection,
                Some(OnionOpenError::Inauthentic),
                "edge {edge}, byte {offset}"
            );
        }
    }
}
