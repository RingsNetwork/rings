use std::collections::HashSet;
use std::time::Instant;

use elliptic_curve::sec1::ToEncodedPoint;
use k256::ProjectivePoint as K256ProjectivePoint;
use rand::distributions::Alphanumeric;
use rand::Rng;
use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::*;

fn random(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn affine_xy(point: Affine) -> ([u8; 32], [u8; 32]) {
    let encoded = point.to_encoded_point(false);
    let bytes = encoded.as_bytes();
    let mut x = [0u8; 32];
    let mut y = [0u8; 32];
    let Some(x_bytes) = bytes.get(1..33) else {
        panic!("missing uncompressed x-coordinate");
    };
    let Some(y_bytes) = bytes.get(33..65) else {
        panic!("missing uncompressed y-coordinate");
    };
    x.copy_from_slice(x_bytes);
    y.copy_from_slice(y_bytes);
    (x, y)
}

fn affine_x(point: Affine) -> [u8; 32] {
    affine_xy(point).0
}

#[test]
fn test_string_to_field() {
    let t: String = random(1024);
    assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);

    let t: String = random(127);
    assert_eq!(field_to_str(&str_to_field(&t)).unwrap(), t);
}

#[test]
fn test_string_to_field_keeps_nul_bytes() {
    let leading_nul = "\0hello";
    assert_eq!(
        field_to_str(&str_to_field(leading_nul)).unwrap(),
        leading_nul
    );

    let chunk_boundary_nul = format!("{}\0tail", "a".repeat(FIELD_CHUNK_SIZE));
    assert_eq!(
        field_to_str(&str_to_field(&chunk_boundary_nul)).unwrap(),
        chunk_boundary_nul
    );
}

#[test]
fn test_bytes_to_field_keeps_binary_payload() {
    let mut payload = vec![0, 255, 1, 2, 3];
    payload.extend((0..FIELD_CHUNK_SIZE * 2).map(|i| (i % 251) as u8));

    assert_eq!(field_to_bytes(&bytes_to_field(&payload)), payload);
}

#[test]
fn test_string_to_affine() {
    let t: String = random(1024);
    assert_eq!(affine_to_str(&str_to_affine(&t).unwrap()).unwrap(), t);

    let t: String = random(127);
    assert_eq!(affine_to_str(&str_to_affine(&t).unwrap()).unwrap(), t);
}

#[test]
fn test_bytes_to_affine() {
    let payload = [0, 1, 2, 3, 255, 128, 0, 42, 77, 0, 99];

    assert_eq!(
        affine_to_bytes(&bytes_to_affine(&payload).unwrap()),
        payload
    );
}

#[test]
fn test_algorithm() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pub_point: Affine = key.pubkey().try_into().unwrap();
    let pub_x = [
        226, 15, 49, 60, 133, 119, 254, 51, 180, 4, 209, 133, 17, 253, 134, 129, 149, 245, 53, 173,
        45, 62, 36, 113, 168, 153, 24, 91, 137, 141, 81, 47,
    ];
    let pub_y = [
        108, 113, 105, 68, 84, 69, 224, 17, 240, 33, 13, 214, 109, 90, 19, 142, 61, 78, 77, 105,
        96, 121, 193, 87, 117, 185, 180, 47, 202, 81, 181, 204,
    ];
    let (got_pub_x, got_pub_y) = affine_xy(pub_point);
    assert_eq!(got_pub_x, pub_x);
    assert_eq!(got_pub_y, pub_y);
    let test = "test";
    let points = str_to_affine(test).unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(affine_to_str(&str_to_affine(test).unwrap()).unwrap(), test);
    let m_point = points[0];
    let r = SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54")
        .unwrap();
    let r_v = [
        31, 146, 117, 219, 175, 223, 186, 129, 148, 46, 179, 51, 11, 7, 243, 140, 190, 228, 235,
        184, 107, 220, 33, 116, 175, 150, 72, 213, 245, 80, 154, 84,
    ];
    let r_sca = GroupScalar::<Secp256k1>::try_from(&r).unwrap().into_inner();
    let mut got_r = [0u8; 32];
    got_r.copy_from_slice(r_sca.to_bytes().as_slice());
    assert_eq!(got_r, r_v);
    let c1 = K256ProjectivePoint::GENERATOR * r_sca;
    let a_c1 = c1.to_affine();
    let c1_x = [
        252, 168, 85, 233, 220, 119, 76, 217, 52, 108, 167, 27, 234, 188, 197, 95, 72, 213, 148,
        212, 111, 255, 6, 59, 9, 134, 111, 121, 175, 9, 189, 105,
    ];
    let c1_y = [
        20, 45, 13, 61, 245, 50, 136, 183, 182, 210, 169, 120, 84, 204, 77, 138, 12, 116, 50, 9,
        115, 98, 138, 245, 24, 61, 223, 144, 55, 180, 231, 59,
    ];
    let (got_c1_x, got_c1_y) = affine_xy(a_c1);
    assert_eq!(got_c1_x, c1_x);
    assert_eq!(got_c1_y, c1_y);

    let mask_point = K256ProjectivePoint::from(pub_point) * r_sca;
    let a_mask = mask_point.to_affine();

    let mask_x = [
        218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
        223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
    ];

    let mask_y = [
        106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154, 11,
        80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
    ];
    let (got_mask_x, got_mask_y) = affine_xy(a_mask);
    assert_eq!(got_mask_x, mask_x);
    assert_eq!(got_mask_y, mask_y);
    let c2 = mask_point + K256ProjectivePoint::from(m_point);
    let c2_y = [
        225, 196, 104, 44, 46, 208, 86, 14, 40, 40, 133, 81, 125, 222, 217, 21, 242, 64, 68, 206,
        194, 27, 61, 193, 20, 18, 110, 198, 39, 60, 214, 200,
    ];
    let c2_x = [
        156, 159, 250, 245, 112, 81, 128, 176, 19, 145, 119, 199, 12, 181, 147, 13, 138, 34, 205,
        124, 119, 235, 28, 243, 77, 11, 100, 13, 159, 164, 188, 247,
    ];
    let a_c2 = c2.to_affine();
    let (got_c2_x, got_c2_y) = affine_xy(a_c2);
    assert_eq!(got_c2_x, c2_x);
    assert_eq!(got_c2_y, c2_y);

    let t = K256ProjectivePoint::from(a_c1)
        * GroupScalar::<Secp256k1>::try_from(&key)
            .unwrap()
            .into_inner();
    let a_t = t.to_affine();
    let t_x = [
        218, 19, 55, 137, 15, 46, 160, 160, 208, 222, 206, 77, 46, 79, 32, 80, 64, 243, 93, 23,
        223, 130, 148, 226, 131, 17, 254, 95, 43, 95, 35, 34,
    ];
    let t_y = [
        106, 127, 47, 58, 214, 6, 110, 28, 171, 176, 73, 11, 34, 28, 125, 10, 82, 154, 84, 154, 11,
        80, 191, 68, 111, 197, 98, 224, 84, 116, 208, 115,
    ];
    let (got_t_x, got_t_y) = affine_xy(a_t);
    assert_eq!(got_t_x, t_x);
    assert_eq!(got_t_y, t_y);

    let ret = c2 - t;
    assert_eq!(affine_x(ret.to_affine()), affine_x(m_point));
}

#[test]
fn test_encrypt_decrypt() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let t: String = random(1024);
    assert_eq!(decrypt(&encrypt(&t, pubkey).unwrap(), &key).unwrap(), t)
}

#[test]
fn test_encrypt_decrypt_keeps_nul_bytes() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let message = format!("\0{}{}", "a".repeat(FIELD_CHUNK_SIZE - 1), "\0tail");
    assert_eq!(
        decrypt(&encrypt(&message, pubkey).unwrap(), &key).unwrap(),
        message
    );
}

#[test]
fn test_encrypt_decrypt_binary_bytes() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let mut rng = Hc128Rng::seed_from_u64(608);
    let mut payload = vec![0, 255, 1, 2, 3];
    payload.extend((0..FIELD_CHUNK_SIZE * 3).map(|i| (i % 251) as u8));

    let ciphertext = encrypt_bytes_with_rng(&payload, pubkey, &mut rng).unwrap();

    assert_eq!(decrypt_bytes(&ciphertext, &key).unwrap(), payload);
}

#[test]
fn test_encrypt_with_rng_is_reproducible_for_same_seed() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let message = format!("prefix\0{}tail", "a".repeat(FIELD_CHUNK_SIZE));
    let mut rng_a = Hc128Rng::seed_from_u64(42);
    let mut rng_b = Hc128Rng::seed_from_u64(42);

    let ciphertext_a = encrypt_with_rng(&message, pubkey, &mut rng_a).unwrap();
    let ciphertext_b = encrypt_with_rng(&message, pubkey, &mut rng_b).unwrap();

    assert_eq!(ciphertext_a, ciphertext_b);
    assert_eq!(decrypt(&ciphertext_a, &key).unwrap(), message);
}

#[test]
fn test_encrypt_uses_fresh_ephemeral_point_per_block() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let message = random(FIELD_CHUNK_SIZE * 4);
    let ciphertext = encrypt(&message, pubkey).unwrap();

    assert!(ciphertext.len() > 1);
    let unique_c1 = ciphertext
        .iter()
        .map(|(c1, _)| c1.0)
        .collect::<HashSet<_>>();
    assert_eq!(unique_c1.len(), ciphertext.len());
}

#[test]
fn test_decrypt_malformed_ciphertext_returns_error() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let malformed = PublicKey([0u8; 33]);
    let result = std::panic::catch_unwind(|| decrypt(&[(malformed, malformed)], &key));

    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
}

#[test]
fn test_aead_encrypt_decrypt_binary_bytes() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let mut rng = Hc128Rng::seed_from_u64(908);
    let mut payload = vec![0, 255, 42, 0, 17];
    payload.extend((0..1024).map(|i| (i % 251) as u8));
    let aad = b"rings-core test associated data";

    let sealed = encrypt_aead_with_rng(&payload, aad, pubkey, &mut rng).unwrap();

    assert_eq!(decrypt_aead(&sealed, aad, &key).unwrap(), payload);
}

#[test]
fn test_aead_rejects_ciphertext_tampering() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let mut rng = Hc128Rng::seed_from_u64(909);
    let mut sealed = encrypt_aead_with_rng(b"authenticated", b"aad", pubkey, &mut rng).unwrap();
    let first = sealed.ciphertext.first_mut().unwrap();
    *first ^= 1;

    assert!(matches!(
        decrypt_aead(&sealed, b"aad", &key),
        Err(Error::MessageDecryptionFailed(_))
    ));
}

#[test]
fn test_aead_rejects_associated_data_tampering() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let mut rng = Hc128Rng::seed_from_u64(910);
    let sealed = encrypt_aead_with_rng(b"authenticated", b"aad", pubkey, &mut rng).unwrap();

    assert!(matches!(
        decrypt_aead(&sealed, b"other-aad", &key),
        Err(Error::MessageDecryptionFailed(_))
    ));
}

#[test]
fn test_aead_rejects_empty_wrapped_key() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let sealed = AeadCiphertext {
        version: AEAD_VERSION,
        encrypted_key: Vec::new(),
        nonce: [0u8; AEAD_NONCE_LEN],
        ciphertext: Vec::new(),
    };

    assert!(matches!(
        decrypt_aead(&sealed, b"aad", &key),
        Err(Error::MessageDecryptionFailed(_))
    ));
}

#[test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn test_bench_encrypt_decrypt_4kb() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();
    let pubkey = key.pubkey();
    let message = random(4 * 1024);
    let rounds = 20;
    let start = Instant::now();

    for _ in 0..rounds {
        let ciphertext = encrypt(std::hint::black_box(&message), pubkey).unwrap();
        let plaintext = decrypt(std::hint::black_box(&ciphertext), &key).unwrap();
        assert_eq!(plaintext, message);
    }

    let elapsed = start.elapsed();
    println!(
        "secp256k1 ElGamal adapter encrypt+decrypt 4KiB: {:?} total, {:?} per round",
        elapsed,
        elapsed / rounds
    );
}
