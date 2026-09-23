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
use crate::onion::sphinx::seed::OnionCarryKey;
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

/// `σ_{k,j} = KDF₃₂(σ_k, relay j)`, `σ_{k,c} = KDF₃₂(σ_k, consumer)` and
/// `k = KDF₄₈(σ_in)` exactly as D7 defines them.
#[test]
fn test_derivation_matches_its_definition() {
    let segment = OnionSegmentSeed::new([7; 32]);
    let seeds = segment.seeds();

    for (index, relay) in seeds.relays.iter().enumerate() {
        let info = [b"relay".as_slice(), &(index as u64 + 1).to_be_bytes()].concat();
        let expected = hkdf::<32>(b"rings-node:onion-carry-seed", &[7; 32], &info);
        assert_eq!(relay.as_bytes(), &expected);
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
    let (segment, _) = OnionSegmentSeed::draw(&mut fixture_rng(30));
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

/// A derived key with a zero subkey `I`, `J` or `L` is rejected by the key constructor that
/// every derivation passes through, so the client re-draws `σ` (a weak HKDF output itself occurs
/// with probability `≈ 2^−128` and cannot be exhibited).
#[test]
fn test_weak_key_is_detected() {
    for (subkey, name) in [(0, Subkey::I), (1, Subkey::J), (2, Subkey::L)] {
        let mut key = [0x5a; KEY_BYTES];
        key[16 * subkey..16 * (subkey + 1)].fill(0);

        assert_eq!(
            OnionCarryKey::new(&key).err(),
            Some(KeyError::ZeroSubkey(name))
        );
    }
    assert!(OnionCarryKey::new(&[0x5a; KEY_BYTES]).is_ok());
}

/// `draw` returns a segment seed all of whose keys are strong, and its keys equal the keys the
/// seed derives afterwards.
#[test]
fn test_draw_returns_a_strong_segment() {
    let (segment, keys) = OnionSegmentSeed::draw(&mut fixture_rng(31));
    let derived = segment.keys().expect("strong segment");

    assert_eq!(
        fingerprint(keys.consumer().aez()),
        fingerprint(derived.consumer().aez())
    );
    assert_eq!(keys.relays().len(), derived.relays().len());
    for (drawn, derived) in keys.relays().iter().zip(derived.relays()) {
        assert_eq!(fingerprint(drawn.aez()), fingerprint(derived.aez()));
    }
}
