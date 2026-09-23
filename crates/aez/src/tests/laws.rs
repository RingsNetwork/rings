//! Algebraic and API laws of the construction, independent of the reference vectors.

use super::witness;
use super::Stream;
use crate::block::Block;
use crate::tbc::Subkeys;
use crate::Aez;
use crate::DecryptError;
use crate::ExpansionExceedsBuffer;
use crate::KeyError;
use crate::Subkey;
use crate::Tweak;
use crate::KEY_BYTES;

/// Byte lengths every sweep covers: all of AEZ-tiny, the tiny/core boundary, and the first
/// few AEZ-core pair counts with every `|M_uv|` residue.
const SWEEP_LENGTHS: core::ops::RangeInclusive<usize> = 0..=130;

/// Expansions exercised by the AE laws: none, sub-block, one block, and multi-block.
const EXPANSIONS: [usize; 6] = [0, 1, 15, 16, 17, 40];

/// A fresh AEZ instance from the stream.
fn keyed(stream: &mut Stream) -> Aez {
    Aez::new(&stream.key()).unwrap()
}

/// A random block.
fn block(stream: &mut Stream) -> Block {
    Block::from_bytes(core::array::from_fn(|_| stream.byte()))
}

witness! {
    /// Law: `2·10^127 = 0^120 ‖ 0x87` (reduction) and `2·X = X << 1` without carry.
    fn doubling_reduces_by_the_field_polynomial() {
        let mut top = [0u8; 16];
        top[0] = 0x80;
        let mut reduced = [0u8; 16];
        reduced[15] = 0x87;
        assert_eq!(Block::from_bytes(top).double().to_bytes(), reduced);
        let mut one = [0u8; 16];
        one[15] = 1;
        let mut two = [0u8; 16];
        two[15] = 2;
        assert_eq!(Block::from_bytes(one).double().to_bytes(), two);
    }

    /// Law: `0·X = 0`, `1·X = X`, `(2k)·X = 2·(k·X)`, `(2k+1)·X = (2k)·X ⊕ X`, and
    /// `k·(X ⊕ Y) = k·X ⊕ k·Y`.
    fn scalar_action_extends_doubling_linearly() {
        let mut stream = Stream::new(0x5eed_0001);
        for _ in 0..64 {
            let (x, y) = (block(&mut stream), block(&mut stream));
            assert!(x.times(0).is_zero());
            assert_eq!(x.times(1).to_bytes(), x.to_bytes());
            for k in 0..40u128 {
                assert_eq!(x.times(2 * k).to_bytes(), x.times(k).double().to_bytes());
                assert_eq!(x.times(2 * k + 1).to_bytes(), (x.times(2 * k) ^ x).to_bytes());
                assert_eq!((x ^ y).times(k).to_bytes(), (x.times(k) ^ y.times(k)).to_bytes());
            }
        }
    }

    /// Law: the `i`-th element of `offsets(j)` is `Δ_{j,i}` in closed form.
    fn offset_stream_matches_the_closed_form() {
        let subkeys = Subkeys::new(&Stream::new(0x5eed_0002).key()).unwrap();
        for j in 0..5u128 {
            for (i, streamed) in (1u32..=70).zip(subkeys.offsets(j)) {
                assert_eq!(streamed.to_bytes(), subkeys.offset(j, i).to_bytes(), "j={j} i={i}");
            }
        }
    }

    /// Law: a key with a zero subkey is rejected, naming the subkey.
    fn zero_subkeys_are_rejected() {
        for (index, subkey) in [Subkey::I, Subkey::J, Subkey::L].into_iter().enumerate() {
            let mut key = [0x42u8; KEY_BYTES];
            key[16 * index..16 * (index + 1)].fill(0);
            assert_eq!(Aez::new(&key).err(), Some(KeyError::ZeroSubkey(subkey)));
        }
    }

    /// Law: `decipher_T ∘ encipher_T = id` and `|encipher_T(X)| = |X|` on every swept
    /// length, for the empty tweak and a nonce-plus-AD tweak.
    fn decipher_inverts_encipher() {
        let mut stream = Stream::new(0x5eed_0003);
        let aez = keyed(&mut stream);
        let (nonce, associated) = (stream.bytes(12), stream.bytes(21));
        let components: [&[u8]; 2] = [&nonce, &associated];
        for tweak in [Tweak::EMPTY, Tweak::new(&components)] {
            for length in SWEEP_LENGTHS {
                let plaintext = stream.bytes(length);
                let mut buffer = plaintext.clone();
                aez.encipher(tweak, &mut buffer);
                assert_eq!(buffer.len(), length);
                aez.decipher(tweak, &mut buffer);
                assert_eq!(buffer, plaintext, "length {length}");
            }
        }
    }

    /// Law: `decrypt(T, τ, encrypt(T, τ, M ‖ s)) = M` for every swept `|M|`, every
    /// expansion, and arbitrary slot content `s`.
    fn decrypt_inverts_encrypt() {
        let mut stream = Stream::new(0x5eed_0004);
        let aez = keyed(&mut stream);
        let nonce = stream.bytes(16);
        let components: [&[u8]; 1] = [&nonce];
        let tweak = Tweak::new(&components);
        for expansion in EXPANSIONS {
            for length in SWEEP_LENGTHS {
                let message = stream.bytes(length);
                let mut buffer = message.clone();
                buffer.extend(stream.bytes(expansion));
                aez.encrypt(tweak, expansion, &mut buffer).unwrap();
                let plaintext = aez.decrypt(tweak, expansion, &mut buffer).unwrap();
                assert_eq!(plaintext, message.as_slice(), "τ={expansion} |M|={length}");
            }
        }
    }

    /// Law: a buffer shorter than `τ` is refused by both directions and left untouched.
    fn buffers_shorter_than_the_expansion_are_refused_untouched() {
        let aez = keyed(&mut Stream::new(0x5eed_0005));
        let original = [7u8; 15];
        let refusal = ExpansionExceedsBuffer { length: 15, expansion: 16 };
        let mut buffer = original;
        assert_eq!(aez.encrypt(Tweak::EMPTY, 16, &mut buffer), Err(refusal));
        assert_eq!(buffer, original);
        assert_eq!(
            aez.decrypt(Tweak::EMPTY, 16, &mut buffer).map(|_| ()),
            Err(DecryptError::Truncated(refusal))
        );
        assert_eq!(buffer, original);
    }

    /// Law: the tweak is a vector: `(A, B)`, `(B, A)` and `(A ‖ B)` encrypt differently.
    fn tweak_components_are_positional() {
        let aez = keyed(&mut Stream::new(0x5eed_0006));
        let (a, b, ab): (&[u8], &[u8], &[u8]) = (b"alpha", b"beta", b"alphabeta");
        let encrypt = |components: &[&[u8]]| {
            let mut buffer = vec![0u8; 48];
            aez.encrypt(Tweak::new(components), 16, &mut buffer).unwrap();
            buffer
        };
        let (forward, backward, joined) = (encrypt(&[a, b]), encrypt(&[b, a]), encrypt(&[ab]));
        assert_ne!(forward, backward);
        assert_ne!(forward, joined);
        assert_ne!(backward, joined);
    }

    /// Law (#834 carry): a value sealed at `τ = 16` for the consumer and enciphered once per
    /// relay key, innermost last, is recovered by the relays deciphering in path order and
    /// the consumer decrypting; every relay step preserves the slot width.
    fn onion_carry_composes() {
        let mut stream = Stream::new(0x5eed_0007);
        for width in [0usize, 1, 15, 16, 17, 31, 32, 33, 64, 100] {
            for relays in 0..4 {
                let consumer = keyed(&mut stream);
                let path: Vec<Aez> = (0..relays).map(|_| keyed(&mut stream)).collect();
                let value = stream.bytes(width);
                let mut slot = value.clone();
                slot.resize(width + 16, 0);
                consumer.encrypt(Tweak::EMPTY, 16, &mut slot).unwrap();
                path.iter().rev().for_each(|relay| relay.encipher(Tweak::EMPTY, &mut slot));
                for relay in &path {
                    relay.decipher(Tweak::EMPTY, &mut slot);
                    assert_eq!(slot.len(), width + 16);
                }
                let recovered = consumer.decrypt(Tweak::EMPTY, 16, &mut slot).unwrap();
                assert_eq!(recovered, value.as_slice(), "width {width}, {relays} relays");
            }
        }
    }

    /// Law (#834 L8): a slot modified at any relay fails the consumer's check.
    fn onion_carry_rejects_a_modified_slot() {
        let mut stream = Stream::new(0x5eed_0008);
        let (consumer, relay) = (keyed(&mut stream), keyed(&mut stream));
        let mut slot = stream.bytes(32 + 16);
        consumer.encrypt(Tweak::EMPTY, 16, &mut slot).unwrap();
        relay.encipher(Tweak::EMPTY, &mut slot);
        slot[5] ^= 0x10;
        relay.decipher(Tweak::EMPTY, &mut slot);
        assert_eq!(
            consumer.decrypt(Tweak::EMPTY, 16, &mut slot).map(|_| ()),
            Err(DecryptError::Inauthentic)
        );
    }
}
