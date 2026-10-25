//! Known-answer vectors captured on `develop` under `elliptic-curve 0.13` (k256/p256 0.13,
//! ecdsa 0.16) before the 0.14 swap (#1051).
//!
//! Law: every byte string the curve stack puts on the wire or derives from fixed inputs is
//! identical across the swap. Each vector was produced by the 0.13 build from the same public
//! API calls these tests make, except the P-256 decompression vectors, which follow SEC1 since
//! #1072 corrected the y-parity tags.

use elliptic_curve::ff::PrimeField;

use super::keccak256;
use super::prime_order::NonIdentityPoint;
use super::prime_order::NonZeroScalar;
use super::signers::secp256r1;
use super::PublicKey;
use super::Secp256k1;
use super::SecretKey;

/// The secp256k1 key of the MetaMask vector in `ecc::tests`.
const SECP256K1_KEY: &str = "65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0";

/// The secp256r1 key of the Web Crypto vector in `signers::secp256r1`.
const SECP256R1_KEY: &str = "2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038";

/// `r ‖ s ‖ v` of `sign_hash(keccak256("rings known answer {i}"))`; `v` covers both parities.
const SECP256K1_SIGNATURES: [&str; 4] = [
    "2e112bba0a211bc863c827028e88abc9ed5e0f0941790493ef0e564a5200ab5b\
     21a80e9448bb28730dcb768c6360d47ed78cd94cb7e6c25df39eb5f613e5026900",
    "38948a20a62eeccba46f93fabdaabcaab09fa4eb3a9f1b611471394c136a72eb\
     128447c7ffe5ce75974e92656d20565bde5b232f8e71b947624b71b5b4d6858100",
    "fb8d1dc812ed81310cb267c1b0d763916506a6170c25bd26282f6624a13df379\
     7de4d37b18986787c8a2fc5cc42c00ef507d143c7e99511152730784d6c631cd00",
    "dae33dfab8242b2f68f9eeb6d705b5fd77a90161bb0654a83b1a6982a60f2912\
     614a4717c57a2e3aba6529562c5936845754caa903b958a659a171bb724c727401",
];

/// The low-s secp256r1 signature of `hash("rings known answer")`.
const SECP256R1_LOW_S: &str = "ed2a21bdcc727033183f235987ed897537a691b21631fcd91247d3dfddf539b8\
                               537a64c9cc8adc6b148527aed8da603f2046f87a308360f9bcbdb73a0b281a87";

/// The same signature with `s ↦ n − s`, i.e. its high-s twin.
const SECP256R1_HIGH_S: &str = "ed2a21bdcc727033183f235987ed897537a691b21631fcd91247d3dfddf539b8\
                                ac859b3533752395eb7ad85127259fc09ca0023376943d8b36fc1388f13b0aca";

/// `(w mod n)·G` in compressed SEC1 for the wide inputs `0x00 0x01 … 0x3f` and `0xff^64`.
const WIDE_RAMP_POINT: &str = "03688f1f5fcb3f9ba024d1ce737ebf51f9d791d1eb220893c03a831ecba990a9e0";
const WIDE_ONES_POINT: &str = "02bf7d0e090ec4e6e0e2c371dcd74439f2d75231821274486ccef45602fd0a7f99";

/// The x-coordinate of the P-256 generator.
const P256_GENERATOR_X: &str = "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296";

/// Uncompressed points of `0x02 ‖ x` and `0x03 ‖ x`: SEC1 maps `0x02` to the even `y` and `0x03`
/// to the odd one, here the SEC 2 generator. The 0.13 build decoded the reverse (#1072).
const P256_DECOMPRESSED: [(u8, &str); 2] = [
    (
        2,
        "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
         b01cbd1c01e58065711814b583f061e9d431cca994cea1313449bf97c840ae0a",
    ),
    (
        3,
        "046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296\
         4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
    ),
];

/// The compressed secp256k1 encoding of `(w mod n)·G`, or `None` when `w ≡ 0`.
fn wide_reduction_point(wide: &[u8; 64]) -> Option<String> {
    NonZeroScalar::<Secp256k1>::from_wide_bytes(wide).map(|scalar| {
        hex::encode(PublicKey::<33>::from(&NonIdentityPoint::generator_mul(&scalar)).0)
    })
}

#[test]
fn test_secp256k1_signatures_match_elliptic_curve_0_13_vectors() {
    let key = SecretKey::try_from(SECP256K1_KEY).unwrap();
    for (index, expected) in SECP256K1_SIGNATURES.iter().enumerate() {
        let hash = keccak256(format!("rings known answer {index}").as_bytes());
        assert_eq!(hex::encode(key.sign_hash(&hash).unwrap()), *expected);
    }
}

#[test]
fn test_secp256r1_signature_and_normalization_match_elliptic_curve_0_13_vectors() {
    let key = SecretKey::try_from(SECP256R1_KEY).unwrap();
    let signature = secp256r1::sign(&key, &secp256r1::hash(b"rings known answer")).unwrap();
    assert_eq!(hex::encode(signature), SECP256R1_LOW_S);

    let high = hex::decode(SECP256R1_HIGH_S).unwrap();
    let s_high: [u8; 32] = high[32..].try_into().unwrap();
    let s_low: [u8; 32] = signature[32..].try_into().unwrap();
    // The vector is a genuine high-s twin: s_high = n − s_low.
    let s_low = p256::Scalar::from_repr(s_low.into()).unwrap();
    assert_eq!(<[u8; 32]>::from((-s_low).to_repr()), s_high);
    assert_eq!(
        hex::encode(secp256r1::normalize_signature(&high).unwrap()),
        SECP256R1_LOW_S
    );
}

#[test]
fn test_wide_reduction_matches_elliptic_curve_0_13_vectors() {
    let mut ramp = [0u8; 64];
    ramp.iter_mut()
        .zip(0u8..)
        .for_each(|(byte, value)| *byte = value);
    assert_eq!(
        wide_reduction_point(&ramp).as_deref(),
        Some(WIDE_RAMP_POINT)
    );
    assert_eq!(
        wide_reduction_point(&[0xff; 64]).as_deref(),
        Some(WIDE_ONES_POINT)
    );
    assert_eq!(wide_reduction_point(&[0; 64]), None);
}

#[test]
fn test_secp256r1_decompression_matches_sec1_vectors() {
    let x = hex::decode(P256_GENERATOR_X).unwrap();
    for (prefix, expected) in P256_DECOMPRESSED {
        let mut compressed = [prefix; 33];
        compressed[1..].copy_from_slice(&x);
        let key = PublicKey(compressed)
            .ct_try_into_secp256r1_pubkey()
            .unwrap()
            .unwrap();
        assert_eq!(hex::encode(key.to_sec1_point(false).as_bytes()), expected);
    }
}
