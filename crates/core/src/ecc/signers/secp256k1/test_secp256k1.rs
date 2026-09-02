use super::*;
use crate::ecc::SecretKey;

#[test]
fn test_default_sign() {
    let key =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
            .unwrap();

    let msg = "hello";
    let h = self::hash(msg.as_bytes());
    let sig = self::sign(&key, &h).unwrap();
    assert_eq!(sig, key.sign(msg).unwrap());
}
