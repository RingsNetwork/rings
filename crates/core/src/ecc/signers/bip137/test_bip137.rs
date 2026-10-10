use super::*;

#[test]
fn test_verify() {
    let pubkey = PublicKey::from_hex_string(
        "026a626503429a973dc4fcde64fa7932158a20c69b79c9eab1245577dd43674dc5",
    )
    .unwrap();

    let msg = "Hello World 42";
    let sig = vec![
        27, 204, 122, 109, 87, 84, 60, 195, 135, 84, 231, 22, 77, 88, 215, 161, 77, 74, 181, 192,
        19, 219, 188, 251, 142, 104, 2, 233, 132, 82, 171, 102, 125, 114, 45, 23, 202, 59, 86, 236,
        76, 169, 164, 164, 179, 221, 206, 54, 32, 106, 81, 115, 217, 42, 93, 114, 131, 115, 128,
        227, 45, 231, 30, 111, 34,
    ];
    assert_eq!(sig.len(), 65);

    let pk = self::recover(msg.as_bytes(), sig).unwrap();
    assert_eq!(pk, pubkey);
    assert_eq!(pk.address(), pubkey.address());
}
