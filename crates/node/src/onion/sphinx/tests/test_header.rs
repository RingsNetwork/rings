//! Header correctness (Prop. header correctness), L5′ lengths and L8 on the header, and the
//! rejection order of [`OnionHeader::peel`].

use rand::RngCore;

use super::fixture_loop;
use super::fixture_rng;
use super::hop_key;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::header::OnionHeaderError;
use crate::onion::sphinx::header::ONION_GROUP_ELEMENT_BYTES;
use crate::onion::sphinx::header::ONION_HEADER_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;
use crate::onion::sphinx::MAX_ONION_LOOP_HOPS;

/// For every `1 ≤ H ≤ Ĥ` and every position `1 ≤ i ≤ H`, `i = H` included, peeling `χ_i` under
/// hop `i`'s key yields exactly `λ_i` (so every `γ_i` verified), and the forwarded header keeps
/// the width `|χ|` (L5′).
#[test]
fn test_every_position_of_every_loop_length_peels_its_own_layer() {
    let mut rng = fixture_rng(10);
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let (keys, layers, route) = fixture_loop(&mut rng, hops);
        let header = OnionHeader::build(&route, &mut rng).expect("build the header");

        let client = keys.iter().zip(layers.iter()).enumerate().fold(
            header,
            |header, (position, (key, layer))| {
                assert_eq!(header.to_bytes().len(), ONION_HEADER_BYTES);
                let peeled = header
                    .peel(key)
                    .unwrap_or_else(|error| panic!("H = {hops}, i = {position}: {error}"));
                assert_eq!(&peeled.layer, layer, "H = {hops}, i = {position}");
                peeled.next
            },
        );

        assert_eq!(client.to_bytes().len(), ONION_HEADER_BYTES);
    }
}

/// L8 on the header: `χ_i ≠ χ_{i+1}` on every edge, in `α` and in `β`, and no forwarded header
/// exposes an all-zero layer block (the filler hides where the loop ends, L5′).
#[test]
fn test_header_bytes_change_on_every_edge_and_show_no_padding() {
    let mut rng = fixture_rng(11);
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let (keys, _, route) = fixture_loop(&mut rng, hops);
        let header = OnionHeader::build(&route, &mut rng).expect("build the header");

        keys.iter().fold(header, |header, key| {
            let next = header.peel(key).expect("peel").next;
            let (before, after) = (header.to_bytes(), next.to_bytes());
            assert_ne!(
                before[..ONION_GROUP_ELEMENT_BYTES],
                after[..ONION_GROUP_ELEMENT_BYTES]
            );
            assert_ne!(
                before[ONION_GROUP_ELEMENT_BYTES..],
                after[ONION_GROUP_ELEMENT_BYTES..]
            );
            assert!(after[ONION_GROUP_ELEMENT_BYTES..ONION_HEADER_BYTES - 16]
                .chunks(ONION_LAYER_BYTES)
                .all(|block| block.iter().any(|byte| *byte != 0)));
            next
        });
    }
}

/// A header is bound to its hop: another hop's key, or one flipped bit of `β` or `γ`, fails the
/// MAC, and nothing is decoded.
#[test]
fn test_wrong_key_or_flipped_bit_fails_the_mac() {
    let mut rng = fixture_rng(12);
    let (keys, _, route) = fixture_loop(&mut rng, 5);
    let header = OnionHeader::build(&route, &mut rng).expect("build the header");

    assert_eq!(header.peel(&hop_key(9)).err(), Some(OnionHeaderError::Mac));
    for bit in [
        8 * ONION_GROUP_ELEMENT_BYTES,
        8 * (ONION_GROUP_ELEMENT_BYTES + ONION_LAYER_BYTES) + 5,
        8 * (ONION_HEADER_BYTES - 16) - 1,
        8 * ONION_HEADER_BYTES - 1,
    ] {
        let mut bytes = header.to_bytes();
        bytes[bit / 8] ^= 1 << (bit % 8);

        assert_eq!(
            OnionHeader::from_bytes(&bytes).peel(&keys[0]).err(),
            Some(OnionHeaderError::Mac),
            "bit {bit}"
        );
    }
}

/// `α` off the curve, or not a compressed point at all, is rejected before any ECDH; a uniformly
/// random header (link cover under `F = 0`) is rejected at `α` or at `γ`.
#[test]
fn test_invalid_group_element_and_random_headers_are_rejected() {
    let mut rng = fixture_rng(13);
    let key = hop_key(0);
    let (_, _, route) = fixture_loop(&mut rng, 1);
    let valid = OnionHeader::build(&route, &mut rng)
        .expect("build the header")
        .to_bytes();

    for alpha in [
        [0_u8; ONION_GROUP_ELEMENT_BYTES],
        [0xff; ONION_GROUP_ELEMENT_BYTES],
    ] {
        let mut bytes = valid;
        bytes[..ONION_GROUP_ELEMENT_BYTES].copy_from_slice(&alpha);
        assert_eq!(
            OnionHeader::from_bytes(&bytes).peel(&key).err(),
            Some(OnionHeaderError::GroupElement)
        );
    }
    for _ in 0..32 {
        let mut bytes = [0_u8; ONION_HEADER_BYTES];
        rng.fill_bytes(&mut bytes);
        let rejection = OnionHeader::from_bytes(&bytes).peel(&key).err();
        assert!(matches!(
            rejection,
            Some(OnionHeaderError::GroupElement | OnionHeaderError::Mac)
        ));
    }
}

/// A loop has between `1` and `Ĥ` positions.
#[test]
fn test_build_rejects_loops_outside_the_header_capacity() {
    let mut rng = fixture_rng(14);
    let (_, _, overlong) = fixture_loop(&mut rng, MAX_ONION_LOOP_HOPS + 1);

    assert_eq!(
        OnionHeader::build(&[], &mut rng).err(),
        Some(OnionHeaderError::HopCount(0))
    );
    assert_eq!(
        OnionHeader::build(&overlong, &mut rng).err(),
        Some(OnionHeaderError::HopCount(MAX_ONION_LOOP_HOPS + 1))
    );
}
