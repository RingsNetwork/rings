//! Header correctness (Prop. header correctness) with the client tag (D6′, #834 H2), class
//! binding (#834 H1), L5′ lengths and L8 on the header, and the rejection order of
//! [`OnionHeader::peel`].

use rand::RngCore;

use super::fixture_keys;
use super::fixture_layer;
use super::fixture_rng;
use super::fixture_route;
use super::hop_key;
use crate::onion::circuit::OnionCellBucket;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::header::OnionHeaderError;
use crate::onion::sphinx::header::OnionHeaderHop;
use crate::onion::sphinx::header::OnionHeaderRoute;
use crate::onion::sphinx::header::OnionLoopTag;
use crate::onion::sphinx::header::ONION_HEADER_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;
use crate::onion::sphinx::MAX_ONION_LOOP_HOPS;

/// Width of `α` in the header encoding.
const ALPHA_BYTES: usize = 33;

/// The loop tag of fixture `seed`.
fn fixture_tag(seed: u64) -> OnionLoopTag {
    let mut tag = [0; 16];
    fixture_rng(seed).fill_bytes(&mut tag);
    OnionLoopTag::new(tag)
}

/// For every `1 ≤ H ≤ Ĥ` and every position `1 ≤ i ≤ H`, `i = H` included, peeling `χ_i` under
/// hop `i`'s key yields exactly `λ_i` (so every `γ_i` verified), the forwarded header keeps the
/// width `|χ|` (L5′), and the header the guard forwards to the client carries `t_⋄` (H2).
#[test]
fn test_every_position_of_every_loop_length_peels_its_own_layer() {
    let class = OnionLoopClass::DEFAULT;
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let seed = u64::try_from(hops).expect("small");
        let keys = fixture_keys(hops);
        let header = OnionHeader::build(
            &fixture_route(seed, &keys),
            class,
            fixture_tag(seed),
            &mut fixture_rng(seed),
        )
        .expect("build the header");

        let client = keys
            .iter()
            .enumerate()
            .fold(header, |header, (position, key)| {
                assert_eq!(header.to_bytes().len(), ONION_HEADER_BYTES);
                let peeled = header
                    .peel(class, key)
                    .unwrap_or_else(|error| panic!("H = {hops}, i = {position}: {error}"));
                assert_eq!(
                    peeled.layer,
                    fixture_layer(seed, position, hops),
                    "H = {hops}, i = {position}"
                );
                peeled.next
            });

        assert_eq!(client.to_bytes().len(), ONION_HEADER_BYTES);
        assert_eq!(client.loop_tag(), fixture_tag(seed), "H = {hops}");
    }
}

/// H1: `γ` binds the class. A hop peeling under the wrong class fails `γ`, and a cell that a
/// colluding hop relabels to another class dies at the next honest hop.
#[test]
fn test_relabelled_cell_dies_at_the_next_honest_hop() {
    let class = OnionLoopClass::DEFAULT;
    let relabelled = OnionLoopClass::try_from(OnionCellBucket::MiB12).expect("a loop class");
    let keys = fixture_keys(5);
    let header = OnionHeader::build(
        &fixture_route(20, &keys),
        class,
        fixture_tag(20),
        &mut fixture_rng(20),
    )
    .expect("build the header");

    assert_eq!(
        header.peel(relabelled, &keys[0]).err(),
        Some(OnionHeaderError::Mac)
    );
    let forwarded = header.peel(class, &keys[0]).expect("honest class").next;
    assert_eq!(
        forwarded.peel(relabelled, &keys[1]).err(),
        Some(OnionHeaderError::Mac)
    );
    assert!(forwarded.peel(class, &keys[1]).is_ok());
}

/// L7 guard closure: one guard key at positions `1` and `H` peels both of its layers.
#[test]
fn test_guard_key_at_first_and_last_position() {
    let class = OnionLoopClass::DEFAULT;
    let mut keys = fixture_keys(4);
    keys.push(hop_key(0));
    let header = OnionHeader::build(
        &fixture_route(21, &keys),
        class,
        fixture_tag(21),
        &mut fixture_rng(21),
    )
    .expect("build the header");

    let client = keys
        .iter()
        .enumerate()
        .fold(header, |header, (position, key)| {
            let peeled = header.peel(class, key).expect("peel");
            assert_eq!(peeled.layer, fixture_layer(21, position, keys.len()));
            peeled.next
        });

    assert_eq!(client.loop_tag(), fixture_tag(21));
}

/// L8 on the header: `χ_i ≠ χ_{i+1}` on every edge, in `α` and in `β`, and no forwarded header
/// exposes an all-zero layer block (the filler hides where the loop ends, L5′).
#[test]
fn test_header_bytes_change_on_every_edge_and_show_no_padding() {
    let class = OnionLoopClass::DEFAULT;
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let seed = 100 + u64::try_from(hops).expect("small");
        let keys = fixture_keys(hops);
        let header = OnionHeader::build(
            &fixture_route(seed, &keys),
            class,
            fixture_tag(seed),
            &mut fixture_rng(seed),
        )
        .expect("build the header");

        keys.iter().fold(header, |header, key| {
            let next = header.peel(class, key).expect("peel").next;
            let (before, after) = (header.to_bytes(), next.to_bytes());
            assert_ne!(before[..ALPHA_BYTES], after[..ALPHA_BYTES]);
            assert_ne!(before[ALPHA_BYTES..], after[ALPHA_BYTES..]);
            assert!(after[ALPHA_BYTES..ONION_HEADER_BYTES - 16]
                .chunks(ONION_LAYER_BYTES)
                .all(|block| block.iter().any(|byte| *byte != 0)));
            next
        });
    }
}

/// A header is bound to its hop: another hop's key, `−α` in place of `α` (same `K_i`, other
/// `SEC1(α)`), or one flipped bit of `β` or `γ` fails the MAC, and nothing is decoded.
#[test]
fn test_wrong_key_negated_alpha_or_flipped_bit_fails_the_mac() {
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(5);
    let header = OnionHeader::build(
        &fixture_route(22, &keys),
        class,
        fixture_tag(22),
        &mut fixture_rng(22),
    )
    .expect("build the header");

    assert_eq!(
        header.peel(class, &hop_key(9)).err(),
        Some(OnionHeaderError::Mac)
    );
    let mut negated = header.to_bytes();
    negated[0] ^= 0x02 ^ 0x03;
    assert_eq!(
        OnionHeader::from_bytes(&negated)
            .expect("header width")
            .peel(class, &keys[0])
            .err(),
        Some(OnionHeaderError::Mac)
    );
    for bit in [
        8 * ALPHA_BYTES,
        8 * (ALPHA_BYTES + ONION_LAYER_BYTES) + 5,
        8 * (ONION_HEADER_BYTES - 16) - 1,
        8 * ONION_HEADER_BYTES - 1,
    ] {
        let mut bytes = header.to_bytes();
        bytes[bit / 8] ^= 1 << (bit % 8);

        assert_eq!(
            OnionHeader::from_bytes(&bytes)
                .expect("header width")
                .peel(class, &keys[0])
                .err(),
            Some(OnionHeaderError::Mac),
            "bit {bit}"
        );
    }
}

/// `α` that is not a compressed point is rejected before any ECDH; a uniformly random header
/// (link cover under `F = 0`) is rejected at `α` or at `γ`; a string of another width is no
/// header.
#[test]
fn test_invalid_group_element_and_random_headers_are_rejected() {
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(1);
    let valid = OnionHeader::build(
        &fixture_route(23, &keys),
        class,
        fixture_tag(23),
        &mut fixture_rng(23),
    )
    .expect("build the header")
    .to_bytes();

    for alpha in [[0_u8; ALPHA_BYTES], [0xff; ALPHA_BYTES]] {
        let mut bytes = valid.clone();
        bytes[..ALPHA_BYTES].copy_from_slice(&alpha);
        assert_eq!(
            OnionHeader::from_bytes(&bytes)
                .expect("header width")
                .peel(class, &keys[0])
                .err(),
            Some(OnionHeaderError::GroupElement)
        );
    }
    let mut rng = fixture_rng(24);
    for _ in 0..32 {
        let mut bytes = vec![0_u8; ONION_HEADER_BYTES];
        rng.fill_bytes(&mut bytes);
        let rejection = OnionHeader::from_bytes(&bytes)
            .expect("header width")
            .peel(class, &keys[0])
            .err();
        assert!(matches!(
            rejection,
            Some(OnionHeaderError::GroupElement | OnionHeaderError::Mac)
        ));
    }
    for width in [ONION_HEADER_BYTES - 1, ONION_HEADER_BYTES + 1] {
        assert_eq!(
            OnionHeader::from_bytes(&vec![0; width]).err(),
            Some(OnionHeaderError::Width(width))
        );
    }
}

/// A loop has between `1` and `Ĥ` positions.
#[test]
fn test_route_rejects_loops_outside_the_header_capacity() {
    let overlong = fixture_keys(MAX_ONION_LOOP_HOPS + 1)
        .iter()
        .map(|key| OnionHeaderHop {
            public_key: key.delegatee_public_key(),
            layer: fixture_layer(25, 0, 2),
        })
        .collect();

    assert_eq!(
        OnionHeaderRoute::new(Vec::new()).err(),
        Some(OnionHeaderError::HopCount(0))
    );
    assert_eq!(
        OnionHeaderRoute::new(overlong).err(),
        Some(OnionHeaderError::HopCount(MAX_ONION_LOOP_HOPS + 1))
    );
}
