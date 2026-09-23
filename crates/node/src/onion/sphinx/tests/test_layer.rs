//! The uniform layer encoding (D6, D6″) and the loop classes (D6, L3).

use rand::Rng;
use rand::RngCore;

use super::fixture_layer;
use super::fixture_rng;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::class::ONION_CELL_FRAMING_BYTES;
use crate::onion::sphinx::header::OnionHeaderMac;
use crate::onion::sphinx::header::ONION_HEADER_BYTES;
use crate::onion::sphinx::header::ONION_HEADER_ROUTING_BYTES;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerError;
use crate::onion::sphinx::layer::ONION_ARGUMENT_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;
use crate::onion::sphinx::MAX_ONION_LOOP_HOPS;

/// The widths are the paper's (D5, D6″): `Ĥ = 14`, `ℓ = 221`, `|β| = 3094`, `|χ| = 3143`.
#[test]
fn test_widths_are_the_specified_constants() {
    assert_eq!(MAX_ONION_LOOP_HOPS, 14);
    assert_eq!(ONION_LAYER_BYTES, 221);
    assert_eq!(ONION_HEADER_ROUTING_BYTES, 3094);
    assert_eq!(ONION_HEADER_BYTES, 3143);
}

/// `decode ∘ encode = Right` for relay and symbol layers alike.
#[test]
fn test_decode_inverts_encode() {
    let mut rng = fixture_rng(1);
    for position in 0..2 {
        let layer = fixture_layer(&mut rng, position, 2);
        let mac = OnionHeaderMac::new(rng.gen());

        let decoded = OnionLayer::decode(&layer.encode(mac)).expect("decode an encoded layer");

        assert_eq!(decoded, (layer, mac));
    }
}

/// `decode(w) = Right(λ, γ) ⇒ encode(λ, γ) = w`: every accepted string is canonical.
#[test]
fn test_accepted_strings_are_canonical() {
    let mut rng = fixture_rng(2);
    let mut accepted = 0_usize;
    for _ in 0..512 {
        let mut bytes = [0_u8; ONION_LAYER_BYTES];
        rng.fill_bytes(&mut bytes);
        // Codes `0..4` cover `relay`, both world-facing symbols, and one code outside `Σ`.
        bytes[0] %= 4;
        if bytes[0] == 0 {
            bytes[1..=ONION_ARGUMENT_BYTES].fill(0);
        }

        if let Ok((layer, mac)) = OnionLayer::decode(&bytes) {
            accepted += 1;
            assert_eq!(*layer.encode(mac), bytes);
        }
    }
    assert!(accepted > 0);
}

/// A code outside `Σ`, and a `relay` layer with arguments, are outside the image of `encode`.
#[test]
fn test_decode_rejects_non_canonical_strings() {
    let mut rng = fixture_rng(3);
    let relay = *fixture_layer(&mut rng, 0, 2).encode(OnionHeaderMac::new([0; 16]));

    let mut unknown = relay;
    unknown[0] = u8::MAX;
    assert_eq!(
        OnionLayer::decode(&unknown).err(),
        Some(OnionLayerError::UnknownSymbol(u8::MAX))
    );

    let mut with_arguments = relay;
    with_arguments[ONION_ARGUMENT_BYTES] = 1;
    assert_eq!(
        OnionLayer::decode(&with_arguments).err(),
        Some(OnionLayerError::RelayArguments)
    );
}

/// `C_b = b − |χ| − F` for every class, `C_16KiB = 13241` (D6″), and the class is the cell length.
#[test]
fn test_carry_width_per_class() {
    assert_eq!(ONION_CELL_FRAMING_BYTES, 0);
    assert_eq!(OnionLoopClass::KiB16.carry_bytes(), 13_241);
    for class in OnionLoopClass::ALL {
        assert_eq!(class.carry_bytes(), class.cell_bytes() - ONION_HEADER_BYTES);
        assert_eq!(class.carry_value_bytes(), class.carry_bytes() - 16);
        assert_eq!(
            OnionLoopClass::from_cell_bytes(class.cell_bytes()),
            Some(class)
        );
    }
    assert_eq!(OnionLoopClass::from_cell_bytes(4 * 1024), None);
    assert!(OnionLoopClass::ALL
        .windows(2)
        .all(|pair| pair[0].cell_bytes() < pair[1].cell_bytes()));
}
