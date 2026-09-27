//! Carry segments (D7): round trip through the keys each hop derives from its own seed, the
//! width law (L3, L5′), and L8 on the carry.

use super::fixture_rng;
use crate::onion::loop_shape::ONION_SEGMENT_RELAYS;
use crate::onion::sphinx::carry;
use crate::onion::sphinx::carry::OnionOpenError;
use crate::onion::sphinx::carry::OnionValueTooWide;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::seed::OnionCarryKey;
use crate::onion::sphinx::seed::OnionSegmentKeys;
use crate::onion::sphinx::seed::OnionSegmentSeed;

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

/// `y_{k,0}`: `value` sealed into a fresh `C_b`-byte slot under `keys`.
fn seal(
    class: OnionLoopClass,
    keys: &OnionSegmentKeys,
    value: &[u8],
) -> Result<Vec<u8>, OnionValueTooWide> {
    let mut slot = vec![0; class.carry_bytes()];
    carry::seal(class, keys, value, slot.as_mut_slice()).map(|()| slot)
}

/// The consumer's view of the slot `y_{k,s}`: its value, opened in place.
fn open(consumer: &OnionCarryKey, slot: Vec<u8>) -> Result<Vec<u8>, OnionOpenError> {
    let width = slot.len();
    carry::open(consumer, slot, 0..width).map(|value| value.as_slice().to_vec())
}

/// `y_{k,0}, …, y_{k,s}`: the slot on every edge from `carry` through `relays`.
fn edges(carry: Vec<u8>, relays: &[OnionCarryKey]) -> Vec<Vec<u8>> {
    core::iter::once(carry.clone())
        .chain(relays.iter().scan(carry, |carry, key| {
            carry::peel(key, carry.as_mut_slice());
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
        let carry = seal(class, &keys, value.as_slice()).expect("seal");
        let slots = edges(carry, relays.as_slice());

        assert_eq!(slots.len(), ONION_SEGMENT_RELAYS + 1);
        assert!(slots.iter().all(|edge| edge.len() == class.carry_bytes()));
        let last = slots.last().expect("the consumer's edge").clone();
        assert_eq!(open(&consumer, last).expect("open"), value);
    }

    let overwide = [widest.as_slice(), &[0]].concat();
    assert_eq!(
        seal(class, &keys, overwide.as_slice()).err(),
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
    let carry = seal(class, &keys, b"one value, one key").expect("seal");
    let slots = edges(carry, relays.as_slice());

    assert!(slots.windows(2).all(|pair| pair[0] != pair[1]));
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
            let mut tampered = slot.clone();
            tampered[offset] ^= 1 << (offset % 8);

            let rejection = edges(tampered, &relays[edge..])
                .pop()
                .map(|slot| open(&consumer, slot).err())
                .expect("the consumer's edge");

            assert_eq!(
                rejection,
                Some(OnionOpenError::Inauthentic),
                "edge {edge}, byte {offset}"
            );
        }
    }
}
