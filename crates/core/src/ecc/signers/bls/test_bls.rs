use super::*;

#[test]
fn test_sign_and_verify() {
    let key = random_sk().unwrap();
    let msg = "hello world";
    let pk = public_key(&key).unwrap();
    let h = hash_to_curve(msg.as_bytes()).unwrap();
    let sig = sign_hash(&key, &h).unwrap();
    assert!(super::verify_hash(vec![h].as_slice(), &sig, vec![pk].as_slice()).unwrap());
    assert!(super::verify(vec![msg.as_bytes()].as_slice(), &sig, vec![pk].as_slice()).unwrap());
}

#[test]
fn test_verification_rejects_wrong_message_key_and_signature() {
    let key = random_sk().unwrap();
    let other_key = random_sk().unwrap();
    let message = b"authorized";
    let other_message = b"different";
    let public = public_key(&key).unwrap();
    let other_public = public_key(&other_key).unwrap();
    let signature = sign(&key, message).unwrap();
    let altered = sign(&other_key, message).unwrap();

    assert!(!verify(&[other_message], &signature, &[public]).unwrap());
    assert!(!verify(&[message], &signature, &[other_public]).unwrap());
    assert!(!verify(&[message], &altered, &[public]).unwrap());
    assert!(matches!(
        verify(&[], &signature, &[]),
        Err(crate::error::Error::BlsInputLengthMismatch)
    ));
    assert!(matches!(
        verify(&[message], &signature, &[]),
        Err(crate::error::Error::BlsInputLengthMismatch)
    ));
    assert!(verify_hash(&[[0xff; 96]], &signature, &[public]).is_err());
    assert!(verify_hash(&[[0; 96]], &Signature([0xff; 96]), &[public]).is_err());
}

#[test]
fn test_hash_result() {
    // this is from hash("hello world") via bls_signature
    // `<https://docs.rs/bls-signatures/latest/bls_signatures/fn.hash.html`>
    let hashed_data: [u8; 96] = [
        138, 203, 106, 10, 25, 0, 11, 120, 167, 254, 109, 207, 27, 42, 63, 46, 108, 179, 30, 196,
        146, 10, 94, 148, 237, 209, 198, 48, 23, 211, 67, 188, 147, 170, 94, 52, 176, 113, 111,
        214, 28, 35, 235, 16, 215, 69, 185, 65, 15, 66, 199, 2, 245, 101, 145, 144, 209, 52, 71,
        179, 27, 209, 127, 155, 231, 9, 235, 11, 82, 89, 83, 171, 47, 179, 253, 128, 26, 104, 238,
        91, 182, 207, 152, 70, 243, 206, 65, 226, 81, 113, 69, 125, 85, 142, 27, 254,
    ];
    let msg = "hello world";
    let h = hash_to_curve(msg.as_bytes()).unwrap();
    assert_eq!(h, hashed_data);
}

#[test]
fn test_aggregate() {
    let key1 = random_sk().unwrap();
    let key2 = random_sk().unwrap();

    let msg1 = "hello alice";
    let msg2 = "hello bob";

    let pk1 = public_key(&key1).unwrap();
    let pk2 = public_key(&key2).unwrap();

    let h1 = hash_to_curve(msg1.as_bytes()).unwrap();
    let h2 = hash_to_curve(msg2.as_bytes()).unwrap();

    let sig1 = sign_hash(&key1, &h1).unwrap();
    let sig2 = sign_hash(&key2, &h2).unwrap();

    let sig_agg = aggregate(&[sig1, sig2]).unwrap();

    assert!(
        super::verify_hash(vec![h1, h2].as_slice(), &sig_agg, vec![pk1, pk2].as_slice()).unwrap()
    );
}

#[test]
fn test_aggregate_rejects_a_missing_member_key() {
    let key1 = random_sk().unwrap();
    let key2 = random_sk().unwrap();
    let messages = [b"first".as_slice(), b"second".as_slice()];
    let pks = [public_key(&key1).unwrap(), public_key(&key2).unwrap()];
    let signatures = [
        sign(&key1, messages[0]).unwrap(),
        sign(&key2, messages[1]).unwrap(),
    ];
    let aggregate = aggregate(&signatures).unwrap();

    assert!(matches!(
        verify(&messages, &aggregate, &pks[..1]),
        Err(crate::error::Error::BlsInputLengthMismatch)
    ));
    assert!(!verify(&messages, &aggregate, &[pks[0], pks[0]]).unwrap());
}

#[test]
fn test_bls_key_derivation_never_fails_and_rejects_identity_and_duplicate_hashes() {
    use rand::SeedableRng;
    use rand_hc::Hc128Rng;

    let mut rng = Hc128Rng::seed_from_u64(0x1255);
    for _ in 0..64 {
        let sk = SecretKey::random_with_rng(&mut rng);
        let pk = public_key(&sk).unwrap();
        let sig = sign(&sk, b"bls-hkdf-derivation").unwrap();
        assert!(verify(&[b"bls-hkdf-derivation".as_slice()], &sig, &[pk]).unwrap());
    }

    // Compressed BLS12-381 G1/G2 point-at-infinity has prefix 0xc0 followed by zeros.
    let mut g1_inf = [0u8; 48];
    g1_inf[0] = 0xc0;
    let mut g2_inf = [0u8; 96];
    g2_inf[0] = 0xc0;
    let valid_sk = random_sk_with_rng(&mut rng).unwrap();
    let valid_pk = public_key(&valid_sk).unwrap();
    let valid_sig = sign(&valid_sk, b"msg").unwrap();
    let valid_h = hash_to_curve(b"msg").unwrap();

    assert!(matches!(
        verify_hash(&[valid_h], &valid_sig, &[PublicKey(g1_inf)]),
        Err(crate::error::Error::IdentityElement)
    ));
    assert!(matches!(
        verify_hash(&[valid_h], &Signature(g2_inf), &[valid_pk]),
        Err(crate::error::Error::IdentityElement)
    ));
    assert!(matches!(
        verify_hash(&[g2_inf], &valid_sig, &[valid_pk]),
        Err(crate::error::Error::IdentityElement)
    ));

    // Rogue-key aggregate verification with duplicate message hashes must return Ok(false).
    let sk2 = random_sk_with_rng(&mut rng).unwrap();
    let pk2 = public_key(&sk2).unwrap();
    let sig_agg = aggregate(&[
        sign_hash(&valid_sk, &valid_h).unwrap(),
        sign_hash(&sk2, &valid_h).unwrap(),
    ])
    .unwrap();
    assert!(!verify_hash(&[valid_h, valid_h], &sig_agg, &[valid_pk, pk2]).unwrap());
}
