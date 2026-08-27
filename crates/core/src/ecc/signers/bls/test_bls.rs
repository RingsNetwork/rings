use super::*;

#[test]
fn test_sign_and_verify() {
    let key = random_sk().unwrap();
    let msg = "hello world";
    let pk = public_key(&key).unwrap();
    let h = hash_to_curve(msg.as_bytes()).unwrap();
    let sig = sign_hash(key, &h).unwrap();
    assert!(super::verify_hash(vec![h].as_slice(), &sig, vec![pk].as_slice()).unwrap());
    assert!(super::verify(vec![msg.as_bytes()].as_slice(), &sig, vec![pk].as_slice()).unwrap());
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

    let sig1 = sign_hash(key1, &h1).unwrap();
    let sig2 = sign_hash(key2, &h2).unwrap();

    let sig_agg = aggregate(&[sig1, sig2]).unwrap();

    assert!(
        super::verify_hash(vec![h1, h2].as_slice(), &sig_agg, vec![pk1, pk2].as_slice()).unwrap()
    );
}
