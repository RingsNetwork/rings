//! Header correctness (Prop. header correctness) with the client tag (D6′, #834 H2), class
//! binding at the cell level (#834 H1), L5′ lengths and L8 on the header, and the rejection
//! outcomes of [`OnionHeader::peel`].

use rand::RngCore;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::PublicKey;

use super::admitted;
use super::charged;
use super::fixture_keys;
use super::fixture_layer;
use super::fixture_rng;
use super::fixture_route;
use super::hop_key;
use crate::onion::circuit::OnionCellBucket;
use crate::onion::loop_shape::MAX_ONION_LOOP_HOPS;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionCellWidth;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::header::OnionHeaderHop;
use crate::onion::sphinx::header::OnionHeaderRoute;
use crate::onion::sphinx::header::OnionHeaderRouteError;
use crate::onion::sphinx::header::OnionLoopTag;
use crate::onion::sphinx::header::OnionPeelError;
use crate::onion::sphinx::header::ONION_HEADER_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;
use crate::onion::sphinx::seed::OnionSegmentSeed;

/// Width of `α` in the header encoding.
const ALPHA_BYTES: usize = 33;

/// The header of fixture `seed` built over `keys` in the default class, with its tag.
fn fixture_header(seed: u64, keys: &[DelegateeKey]) -> (OnionHeader, OnionLoopTag) {
    OnionHeader::build(
        &fixture_route(seed, keys),
        OnionLoopClass::DEFAULT,
        &mut fixture_rng(seed),
    )
    .expect("build the header")
}

/// A header's bytes decoded back, as the cell parser does.
fn decode(bytes: &[u8]) -> OnionHeader {
    OnionHeader::decode(bytes).expect("header width")
}

/// For every `1 ≤ H ≤ Ĥ` and every position `1 ≤ i ≤ H`, `i = H` included, peeling `χ_i` under
/// hop `i`'s key yields exactly `λ_i` (so every `γ_i` verified), the forwarded header keeps the
/// width `|χ|` (L5′), and the header the guard forwards to the client carries the `t_⋄` that
/// `build` drew (H2).
#[test]
fn test_every_position_of_every_loop_length_peels_its_own_layer() {
    let class = OnionLoopClass::DEFAULT;
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let seed = u64::try_from(hops).expect("small");
        let keys = fixture_keys(hops);
        let (header, tag) = fixture_header(seed, &keys);

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
        assert_eq!(client.loop_tag(), tag, "H = {hops}");
    }
}

/// H1 at the cell level: a colluding hop that re-pads a 16 KiB cell to 12 MiB forwards a cell
/// whose parsed class is the new length, and the next honest hop rejects it as invalid; the
/// honest cell passes the same hop.
#[test]
fn test_relabelled_cell_dies_at_the_next_honest_hop() {
    let keys = fixture_keys(5);
    let mut rng = fixture_rng(20);
    let (_, segment_keys) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let (cell, _) = OnionCell::client(
        &fixture_route(20, &keys),
        OnionLoopClass::DEFAULT,
        &segment_keys,
        b"value",
        &mut rng,
    )
    .expect("client cell");
    let OnionStep::Relayed {
        cell: forwarded, ..
    } = admitted(cell.into_bytes(), &keys[0])
        .step()
        .expect("strong key")
    else {
        panic!("position 1 is a relay");
    };
    let honest = forwarded.into_bytes();
    let large = OnionLoopClass::try_from(OnionCellBucket::MiB12).expect("a loop class");
    let mut relabelled = honest.clone();
    relabelled.resize(large.cell_bytes(), 0);

    let relabelled = charged(relabelled);
    assert_eq!(relabelled.value().class(), large);
    assert_eq!(
        relabelled
            .peel(&keys[1])
            .err()
            .map(|error| error.to_string()),
        Some(OnionPeelError::Invalid.to_string())
    );
    assert!(charged(honest).peel(&keys[1]).is_ok());
}

/// A string whose length is no class is no cell, and `into_bytes ∘ parse = id` on cells.
#[test]
fn test_cell_parser_admits_exactly_the_class_lengths() {
    for width in [
        0,
        ONION_HEADER_BYTES,
        4 * 1024,
        16 * 1024 - 1,
        16 * 1024 + 1,
    ] {
        assert_eq!(
            OnionCell::parse(vec![0; width]).err(),
            Some(OnionCellWidth(width))
        );
    }
    let mut bytes = vec![0; 16 * 1024];
    fixture_rng(26).fill_bytes(&mut bytes);
    let cell = OnionCell::parse(bytes.clone()).expect("a 16 KiB cell");
    assert_eq!(cell.class(), OnionLoopClass::DEFAULT);
    assert_eq!(cell.into_bytes(), bytes);
}

/// L7 guard closure: one guard key at positions `1` and `H` peels both of its layers, and the
/// tag reaches the client.
#[test]
fn test_guard_key_at_first_and_last_position() {
    let class = OnionLoopClass::DEFAULT;
    let mut keys = fixture_keys(4);
    keys.push(hop_key(0));
    let (header, tag) = fixture_header(21, &keys);

    let client = keys
        .iter()
        .enumerate()
        .fold(header, |header, (position, key)| {
            let peeled = header.peel(class, key).expect("peel");
            assert_eq!(peeled.layer, fixture_layer(21, position, keys.len()));
            peeled.next
        });

    assert_eq!(client.loop_tag(), tag);
}

/// L8 on the header: `χ_i ≠ χ_{i+1}` on every edge, in `α` and in `β`, and no forwarded header
/// exposes an all-zero layer block (the filler hides where the loop ends, L5′).
#[test]
fn test_header_bytes_change_on_every_edge_and_show_no_padding() {
    let class = OnionLoopClass::DEFAULT;
    for hops in 1..=MAX_ONION_LOOP_HOPS {
        let seed = 100 + u64::try_from(hops).expect("small");
        let keys = fixture_keys(hops);
        let (header, _) = fixture_header(seed, &keys);

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
/// `SEC1(α)`), or one flipped bit of `β` or `γ` makes it invalid, and nothing is decoded.
#[test]
fn test_wrong_key_negated_alpha_or_flipped_bit_is_invalid() {
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(5);
    let (header, _) = fixture_header(22, &keys);

    assert_eq!(
        header.peel(class, &hop_key(9)).err(),
        Some(OnionPeelError::Invalid)
    );
    let mut negated = header.to_bytes();
    negated[0] ^= 0x02 ^ 0x03;
    assert_eq!(
        decode(&negated).peel(class, &keys[0]).err(),
        Some(OnionPeelError::Invalid)
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
            decode(&bytes).peel(class, &keys[0]).err(),
            Some(OnionPeelError::Invalid),
            "bit {bit}"
        );
    }
}

/// `α` that is not a compressed point, and a uniformly random header (link cover under
/// `F = 0`), are one outcome: invalid.
#[test]
fn test_invalid_group_element_and_random_headers_are_invalid() {
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(1);
    let valid = fixture_header(23, &keys).0.to_bytes();

    for alpha in [[0_u8; ALPHA_BYTES], [0xff; ALPHA_BYTES]] {
        let mut bytes = valid.clone();
        bytes[..ALPHA_BYTES].copy_from_slice(&alpha);
        assert_eq!(
            decode(&bytes).peel(class, &keys[0]).err(),
            Some(OnionPeelError::Invalid)
        );
    }
    let mut rng = fixture_rng(24);
    for _ in 0..32 {
        let mut bytes = vec![0_u8; ONION_HEADER_BYTES];
        rng.fill_bytes(&mut bytes);
        assert_eq!(
            decode(&bytes).peel(class, &keys[0]).err(),
            Some(OnionPeelError::Invalid)
        );
    }
}

/// A loop has between `1` and `Ĥ` positions, each keyed by a point.
#[test]
fn test_route_rejects_bad_lengths_and_keys() {
    let hop = |key: PublicKey<33>| OnionHeaderHop {
        public_key: key,
        layer: fixture_layer(25, 0, 2),
    };
    let overlong = fixture_keys(MAX_ONION_LOOP_HOPS + 1)
        .iter()
        .map(|key| hop(key.delegatee_public_key()))
        .collect();

    assert_eq!(
        OnionHeaderRoute::new(Vec::new()).err(),
        Some(OnionHeaderRouteError::HopCount(0))
    );
    assert_eq!(
        OnionHeaderRoute::new(overlong).err(),
        Some(OnionHeaderRouteError::HopCount(MAX_ONION_LOOP_HOPS + 1))
    );
    assert_eq!(
        OnionHeaderRoute::new(vec![hop(PublicKey([0; 33]))]).err(),
        Some(OnionHeaderRouteError::PublicKey)
    );
}
