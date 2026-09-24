//! Seeds (D7): the derivation matches its HKDF definition, domain separation keeps every seed
//! distinct, and a weak key is detected.

use hkdf::Hkdf;
use rings_aez::Aez;
use rings_aez::KeyError;
use rings_aez::Subkey;
use rings_aez::Tweak;
use rings_aez::KEY_BYTES;
use sha2::Sha256;

use super::fixture_rng;
use crate::onion::sphinx::seed::first_strong;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;

/// `HKDF-SHA256(salt, ikm, info)[0, N)`, written out independently of the module under test.
fn hkdf<const N: usize>(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; N] {
    let mut okm = [0; N];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut okm)
        .expect("HKDF length");
    okm
}

/// `π_k(0^64)` under a keyed cipher: equal keys give equal permutations.
fn fingerprint(cipher: &Aez) -> Vec<u8> {
    let mut block = vec![0; 64];
    cipher.encipher(Tweak::EMPTY, block.as_mut_slice());
    block
}

/// `σ_{k,j} = KDF₃₂(σ_k, relay ‖ j)`, `σ_{k,c} = KDF₃₂(σ_k, consumer)` and
/// `k = KDF₄₈(σ_in)` exactly as D7 defines them.
#[test]
fn test_derivation_matches_its_definition() {
    let segment = OnionSegmentSeed::new([7; 32]);
    let seeds = segment.seeds();

    for (relay, seed) in (1_u8..).zip(seeds.relays.iter()) {
        let info = [b"relay".as_slice(), &[relay]].concat();
        let expected = hkdf::<32>(b"rings-node:onion-carry-seed", &[7; 32], &info);
        assert_eq!(seed.as_bytes(), &expected);
    }
    let consumer = hkdf::<32>(b"rings-node:onion-carry-seed", &[7; 32], b"consumer");
    assert_eq!(seeds.consumer.as_bytes(), &consumer);

    let key = hkdf::<KEY_BYTES>(b"rings-node:onion-carry-key", &consumer, b"aez");
    assert_eq!(
        fingerprint(seeds.consumer.key().expect("strong key").aez()),
        fingerprint(&Aez::new(&key).expect("strong key"))
    );
}

/// Domain separation: the `s` relay seeds, the consumer seed and the segment seed are pairwise
/// distinct, so no two hops of a segment hold the same key.
#[test]
fn test_segment_seeds_are_pairwise_distinct() {
    let (segment, _) = OnionSegmentSeed::draw(&mut fixture_rng(30)).expect("strong segment");
    let seeds = segment.seeds();
    let all = seeds
        .relays
        .iter()
        .chain([&seeds.consumer])
        .map(OnionCarrySeed::as_bytes)
        .chain([segment.as_bytes()])
        .collect::<Vec<_>>();

    for (index, seed) in all.iter().enumerate() {
        assert!(all[index + 1..].iter().all(|other| other != seed));
    }
}

/// `first_strong` returns the first candidate whose derivation succeeds, re-drawing after each
/// weak one, and fails closed with the last error after `SEGMENT_SEED_DRAWS = 4` weak draws. The
/// derivation is injected, since a weak HKDF output (probability `≈ 2^−125`) cannot be exhibited.
#[test]
fn test_first_strong_redraws_and_fails_closed() {
    let weak = KeyError::ZeroSubkey(Subkey::J);
    for strong_from in 0..6_u8 {
        let mut next = 0_u8;
        let drawn = first_strong(
            || {
                next += 1;
                next - 1
            },
            |candidate| {
                (*candidate >= strong_from)
                    .then_some(*candidate)
                    .ok_or(weak)
            },
        );

        if strong_from < 4 {
            assert_eq!(drawn, Ok((strong_from, strong_from)));
        } else {
            assert_eq!(drawn, Err(weak));
        }
        assert_eq!(next, strong_from.saturating_add(1).min(4));
    }
}

/// `draw` returns a segment seed all of whose keys are strong, and its keys equal the keys the
/// seed derives afterwards.
#[test]
fn test_draw_returns_a_strong_segment() {
    let (segment, keys) = OnionSegmentSeed::draw(&mut fixture_rng(31)).expect("strong segment");
    let derived = segment.keys().expect("strong segment");

    assert_eq!(
        fingerprint(keys.consumer().aez()),
        fingerprint(derived.consumer().aez())
    );
    for (drawn, derived) in keys.relays().iter().zip(derived.relays()) {
        assert_eq!(fingerprint(drawn.aez()), fingerprint(derived.aez()));
    }
}
