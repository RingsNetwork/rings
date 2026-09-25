//! The uniform layer encoding (D6, D6″) and the loop classes (D6, L3).

use rand::RngCore;
use subtle::ConstantTimeEq;

use super::fixture_layer;
use super::fixture_rng;
use crate::onion::circuit::OnionCellBucket;
use crate::onion::loop_shape::MAX_ONION_LOOP_HOPS;
use crate::onion::signature::ONION_SIGNATURE;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::class::ONION_CELL_FRAMING_BYTES;
use crate::onion::sphinx::header::OnionHeaderMac;
use crate::onion::sphinx::header::ONION_HEADER_BYTES;
use crate::onion::sphinx::header::ONION_HEADER_ROUTING_BYTES;
use crate::onion::sphinx::layer::OnionLayer;
use crate::onion::sphinx::layer::OnionLayerError;
use crate::onion::sphinx::layer::ONION_ARGUMENT_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;

/// The widths are the specified ones (D5, D6″ with #834 H2): `Ĥ = 14`, `ℓ = 205`, `|β| = 2870`,
/// `|χ| = 2919`.
#[test]
fn test_widths_are_the_specified_constants() {
    assert_eq!(MAX_ONION_LOOP_HOPS, 14);
    assert_eq!(ONION_LAYER_BYTES, 205);
    assert_eq!(ONION_HEADER_ROUTING_BYTES, 2870);
    assert_eq!(ONION_HEADER_BYTES, 2919);
}

/// `decode ∘ encode = Right` for relay and symbol layers alike.
#[test]
fn test_decode_inverts_encode() {
    for position in 0..2 {
        let mac = OnionHeaderMac::new([u8::try_from(position).expect("small"); 16]);

        let (layer, decoded_mac) =
            OnionLayer::decode(fixture_layer(1, position, 2).encode(&mac).as_slice())
                .expect("decode an encoded layer");

        assert_eq!(layer, fixture_layer(1, position, 2));
        assert!(bool::from(decoded_mac.ct_eq(&mac)));
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
        // Codes `0..=|Σ|` cover every symbol and the first code outside `Σ`.
        bytes[0] %= u8::try_from(ONION_SIGNATURE.symbols().len() + 1).expect("Σ fits a byte");
        if bytes[0] == 0 {
            bytes[1..=ONION_ARGUMENT_BYTES].fill(0);
        }

        if let Ok((layer, mac)) = OnionLayer::decode(&bytes) {
            accepted += 1;
            assert_eq!(*layer.encode(&mac), bytes);
        }
    }
    assert!(accepted > 0);
}

/// A code outside `Σ`, a `relay` layer with arguments, and a string of another width are
/// outside the image of `encode`.
#[test]
fn test_decode_rejects_non_canonical_strings() {
    let relay = *fixture_layer(3, 0, 2).encode(&OnionHeaderMac::new([0; 16]));

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

    for width in [ONION_LAYER_BYTES - 1, ONION_LAYER_BYTES + 1] {
        assert_eq!(
            OnionLayer::decode(&vec![0; width]).err(),
            Some(OnionLayerError::Width(width))
        );
    }
}

/// Every bucket but `KiB4` is a class with `C_b = b − |χ| − F` (`C_16KiB = 13465`), the class is
/// the cell length, and distinct classes have distinct MAC labels.
#[test]
fn test_carry_width_per_class() {
    assert_eq!(ONION_CELL_FRAMING_BYTES, 0);
    assert_eq!(OnionLoopClass::DEFAULT.carry_bytes(), 13_465);
    assert!(OnionLoopClass::try_from(OnionCellBucket::KiB4).is_err());
    let classes = OnionCellBucket::ALL
        .into_iter()
        .filter_map(|bucket| OnionLoopClass::try_from(bucket).ok())
        .collect::<Vec<_>>();

    assert_eq!(classes.len(), OnionCellBucket::ALL.len() - 1);
    for class in classes.iter().copied() {
        assert_eq!(class.carry_bytes(), class.cell_bytes() - ONION_HEADER_BYTES);
        assert_eq!(class.carry_value_bytes(), class.carry_bytes() - 16);
        assert_eq!(
            OnionLoopClass::from_cell_bytes(class.cell_bytes()),
            Some(class)
        );
    }
    assert_eq!(OnionLoopClass::from_cell_bytes(4 * 1024), None);
    for (index, class) in classes.iter().enumerate() {
        assert!(classes[index + 1..]
            .iter()
            .all(|other| other.mac_label() != class.mac_label()));
    }
}
