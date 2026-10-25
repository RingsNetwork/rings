//! SEC1 §2.3.3 conformance of the 33-byte key form on secp256k1 and P-256 (#1072).
//!
//! Law: for every point `(x, y) ≠ O`, `from_u8(x ‖ y)` is the library's `to_sec1_point(true)`,
//! `0x02 ‖ x` for an even `y` and `0x03 ‖ x` for an odd one, and decoding it gives `(x, y)` back.
//! The external vectors are the generators of SEC 2 (v2, §2.4.1 and §2.4.2), which list `G` in
//! compressed form, their negations `−G = (x, p − y)` of the other parity, and the P-256 key
//! of RFC 6979 §A.2.5.

use elliptic_curve::sec1::ToSec1Point;

use super::PublicKey;
use super::SecretKey;

/// An external SEC1 vector: the uncompressed `x ‖ y` of a point and its compressed encoding.
struct Sec1Vector {
    /// `x ‖ y`, 64 bytes in hex.
    uncompressed: &'static str,
    /// `t ‖ x`, 33 bytes in hex.
    compressed: &'static str,
}

/// secp256k1: `G` of SEC 2 §2.4.1 (even `y`) and `−G` (odd `y`).
const SECP256K1_VECTORS: [Sec1Vector; 2] = [
    Sec1Vector {
        uncompressed: "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798\
                       483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8",
        compressed: "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
    },
    Sec1Vector {
        uncompressed: "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798\
                       b7c52588d95c3b9aa25b0403f1eef75702e84bb7597aabe663b82f6f04ef2777",
        compressed: "0379be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
    },
];

/// P-256: `G` of SEC 2 §2.4.2 (odd `y`), `−G` (even `y`) and the RFC 6979 §A.2.5 key `U`
/// (odd `y`).
const SECP256R1_VECTORS: [Sec1Vector; 3] = [
    Sec1Vector {
        uncompressed: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
                       4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
        compressed: "036b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    },
    Sec1Vector {
        uncompressed: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
                       b01cbd1c01e58065711814b583f061e9d431cca994cea1313449bf97c840ae0a",
        compressed: "026b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    },
    Sec1Vector {
        uncompressed: "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6\
                       7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299",
        compressed: "0360fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6",
    },
];

/// The RFC 6979 §A.2.5 P-256 private key, whose public key is the last of
/// [`SECP256R1_VECTORS`].
const RFC6979_P256_KEY: &str = "c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721";

/// The `x ‖ y` of an uncompressed SEC1 encoding `0x04 ‖ x ‖ y`.
fn untagged(encoded: &[u8]) -> &[u8] {
    assert_eq!(encoded.first(), Some(&0x04));
    &encoded[1..]
}

/// The P-256 point a 33-byte key decodes to, as `0x04 ‖ x ‖ y`.
fn secp256r1_uncompressed(key: PublicKey<33>) -> Option<Vec<u8>> {
    Option::<p256::AffinePoint>::from(key.ct_into_secp256r1_affine())
        .map(|point| point.to_sec1_point(false).as_bytes().to_vec())
}

/// The secp256k1 point a 33-byte key decodes to, as `0x04 ‖ x ‖ y`.
fn secp256k1_uncompressed(key: PublicKey<33>) -> Option<Vec<u8>> {
    k256::PublicKey::try_from(key)
        .ok()
        .map(|point| point.to_sec1_point(false).as_bytes().to_vec())
}

/// Check one curve's vectors: compression by `from_u8` and decoding by `decode`.
fn check_vectors(vectors: &[Sec1Vector], decode: fn(PublicKey<33>) -> Option<Vec<u8>>) {
    for vector in vectors {
        let uncompressed = hex::decode(vector.uncompressed).unwrap();
        let key = PublicKey::from_u8(&uncompressed).unwrap();
        assert_eq!(hex::encode(key.0), vector.compressed);
        assert_eq!(
            decode(PublicKey::from_hex_string(vector.compressed).unwrap())
                .as_deref()
                .map(untagged),
            Some(uncompressed.as_slice())
        );
    }
}

#[test]
fn test_secp256k1_compression_matches_sec2_vectors() {
    check_vectors(&SECP256K1_VECTORS, secp256k1_uncompressed);
}

#[test]
fn test_secp256r1_compression_matches_sec2_and_rfc6979_vectors() {
    check_vectors(&SECP256R1_VECTORS, secp256r1_uncompressed);
}

#[test]
fn test_rfc6979_p256_key_derives_its_published_public_key() {
    let secret: elliptic_curve::FieldBytes<p256::NistP256> =
        (&SecretKey::try_from(RFC6979_P256_KEY).unwrap()).into();
    let signing_key = ecdsa::SigningKey::<p256::NistP256>::from_bytes(&secret).unwrap();
    let encoded = signing_key.verifying_key().to_sec1_point(false);
    let vector = SECP256R1_VECTORS.last().unwrap();
    assert_eq!(
        hex::encode(untagged(encoded.as_bytes())),
        vector.uncompressed
    );
}

#[test]
fn test_compression_round_trips_with_library_sec1_encoding() {
    let mut parities = [[false; 2]; 2];
    for seed in 1u8..=16 {
        let secret = SecretKey::try_from(hex::encode([seed; 32]).as_str()).unwrap();

        let k1 = k256::PublicKey::from_secret_scalar(&secret.0.to_nonzero_scalar());
        let k1_uncompressed = k1.to_sec1_point(false);
        let k1_key = PublicKey::from_u8(untagged(k1_uncompressed.as_bytes())).unwrap();
        assert_eq!(&k1_key.0[..], k1.to_sec1_point(true).as_bytes());
        assert_eq!(k1_key, secret.pubkey());
        assert_eq!(
            secp256k1_uncompressed(k1_key).as_deref(),
            Some(k1_uncompressed.as_bytes())
        );

        let field: elliptic_curve::FieldBytes<p256::NistP256> = (&secret).into();
        let r1 = *ecdsa::SigningKey::<p256::NistP256>::from_bytes(&field)
            .unwrap()
            .verifying_key();
        let r1_uncompressed = r1.to_sec1_point(false);
        let r1_key = PublicKey::from_u8(untagged(r1_uncompressed.as_bytes())).unwrap();
        assert_eq!(&r1_key.0[..], r1.to_sec1_point(true).as_bytes());
        assert_eq!(
            secp256r1_uncompressed(r1_key).as_deref(),
            Some(r1_uncompressed.as_bytes())
        );

        parities[0][usize::from(k1_key.0[0] - 2)] = true;
        parities[1][usize::from(r1_key.0[0] - 2)] = true;
    }
    // The seeds reach both tags on both curves, so the law is checked on each parity.
    assert_eq!(parities, [[true; 2]; 2]);
}

#[test]
fn test_secp256r1_decoding_rejects_non_compressed_tags() {
    let x = &hex::decode(SECP256R1_VECTORS[0].compressed).unwrap()[1..];
    for tag in [0x00, 0x01, 0x04, 0x05, 0x06, 0x07, 0xff] {
        let mut key = [tag; 33];
        key[1..].copy_from_slice(x);
        assert_eq!(
            secp256r1_uncompressed(PublicKey(key)),
            None,
            "tag {tag:#04x}"
        );
    }
}
