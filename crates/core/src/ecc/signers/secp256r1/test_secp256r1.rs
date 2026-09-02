use super::*;
use crate::ecc::SecretKey;

/// The step to get pk and sk from js console:
/// help functions like byteArrayToHex, concatUint8Arrays and BaseUrlToUint8Array are note included.:
///
/// ```js
/// let keyPair = await window.crypto.subtle.generateKey(
/// {
///   name: "ECDSA",
///   namedCurve: "P-256",
/// },
///   true,
///   ["sign", "verify"],
/// );
///
/// jwk = await window.crypto.subtle.exportKey("jwk", keyPair.privateKey);
/// let sk = byteArrayToHex(base64UrlToUint8Array(jwk.d))
/// >>> "2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038"
///
/// let pk =  byteArrayToHex(concatUint8Arrays(base64UrlToUint8Array(jwk.x), base64UrlToUint8Array(jwk.y)))
/// >>> "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702"
///
/// function messageWithPrefixToBytes(msg) {
///     const prefix = "\x19Rings Signed Message:\n" + msg.length;
///     const encoder = new TextEncoder();
///
///     const prefixBytes = encoder.encode(prefix);
///     const msgBytes = encoder.encode(msg);
///
///     const combined = new Uint8Array(prefixBytes.length + msgBytes.length);
///     combined.set(prefixBytes);
///     combined.set(msgBytes, prefixBytes.length);
///
///     return combined;
/// }
///
/// let encoded = messageToBytes("hello world")
///
/// let signature = await window.crypto.subtle.sign(
///   {
///    name: "ECDSA",
///    hash: { name: "SHA-256" },
///    namedCurve: "P-256"
///   },
///   keyPair.privateKey,
///   encoded,
/// );
/// byteArrayToHex(new Uint8Array(signature))
/// >>> 43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bf4fe1343f0b08a810768475fa87fd061a586e943ca9665ee167a3f63c70c72fd9
/// ```
#[test]
fn test_secp256r1_sign_and_verify() {
    let pk: PublicKey<33> = PublicKey::<33>::from_hex_string(
	    "17a6afd392fcbe4ac9270a599a9c5732c4f838ce35ea2234d389d8f0c367f3f5dcab906352e27289002c7f2c96039ddce7c1b5aad8b87ba94984d4c8b4f95702"
	).unwrap();
    let sk =
        SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")
            .unwrap();
    // Check msg encode
    let msg = "hello world";
    let prefix_msg = magic_prefix(msg.as_bytes());
    let js_msg =
        hex::decode("1952696e6773205369676e6564204d6573736167653a0a313168656c6c6f20776f726c64")
            .unwrap()
            .to_vec();
    assert_eq!(prefix_msg, js_msg, "encoded msg not equal");
    let sig: [u8; 64] = hex::decode("43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bf4fe1343f0b08a810768475fa87fd061a586e943ca9665ee167a3f63c70c72fd9").unwrap().try_into().unwrap();

    // Check our sign and verify work right
    let our_sig = sign(&sk, &hash(msg.as_bytes())).unwrap();
    assert!(verify(msg.as_bytes(), &pk.address(), our_sig, &pk));

    let hash_msg: [u8; 32] =
        hex::decode("5e230abb2ae1cb0717986854d6e16b998da03b827b736c9ac32f6ec9e47e3670")
            .unwrap()
            .try_into()
            .unwrap();
    let hashed = hash(msg.as_bytes());
    assert_eq!(hashed, hash_msg, "hash ret not equal");

    assert!(verify(msg.as_bytes(), &pk.address(), sig, &pk));
}

#[test]
fn test_secp256r1_rejects_high_s_signature() -> Result<()> {
    let sk =
        SecretKey::try_from("2544acda37415a476d42312969926dc48e529867036cec71922d4177ea9c1038")?;
    let sk_bytes: FieldBytes<p256::NistP256> = (&sk).into();
    let signing_key = ecdsa::SigningKey::<p256::NistP256>::from_bytes(&sk_bytes)?;
    let encoded = signing_key.verifying_key().to_encoded_point(false);
    let pk = PublicKey::from_u8(
        encoded
            .as_bytes()
            .get(1..)
            .ok_or(Error::PublicKeyBadFormat)?,
    )?;
    let msg = b"canonical signature";
    let low_s = ecdsa::Signature::<p256::NistP256>::from_slice(&sign(&sk, &hash(msg))?)?;
    let (r, s) = low_s.split_scalars();
    let high_s = ecdsa::Signature::<p256::NistP256>::from_scalars(r.to_bytes(), (-s).to_bytes())?;

    assert!(super::super::ecdsa_signature_s_is_high(&high_s));
    assert!(!verify(msg, &pk.address(), high_s.to_bytes(), &pk));
    Ok(())
}

#[test]
fn test_invalid_public_key_does_not_verify() {
    let pk = PublicKey([0u8; 33]);
    let sig = [0u8; 64];

    assert!(!verify(b"msg", &pk.address(), sig, &pk));
}
