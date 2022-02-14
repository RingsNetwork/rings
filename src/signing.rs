use web3::signing::{keccak256, recover, Key};
use web3::types::{Address, Bytes, SignedData, H256};

pub fn sign<M>(message: M, key: impl Key) -> SignedData
where
    M: AsRef<[u8]>,
{
    let message = message.as_ref();
    let message_hash: H256 = keccak256(message).into();

    let signature = key
        .sign_message(message_hash.as_bytes())
        .expect("hash is non-zero 32-bytes; qed");
    let v = signature
        .v
        .try_into()
        .expect("signature recovery in electrum notation always fits in a u8");

    let signature_bytes = Bytes({
        let mut bytes = Vec::with_capacity(65);
        bytes.extend_from_slice(signature.r.as_bytes());
        bytes.extend_from_slice(signature.s.as_bytes());
        bytes.push(v);
        bytes
    });

    // We perform this allocation only after all previous fallible actions have completed successfully.
    let message = message.to_owned();

    SignedData {
        message,
        message_hash,
        v,
        r: signature.r,
        s: signature.s,
        signature: signature_bytes,
    }
}

pub fn verify<M, S>(message: M, address: Address, signature: S) -> bool
where
    M: AsRef<[u8]>,
    S: AsRef<[u8]>,
{
    // length r: 32, length s: 32, length v(recovery_id): 1
    let sig_ref = signature.as_ref();
    if sig_ref.len() != 65 {
        return false;
    }

    let r_s_signature = &sig_ref[..64];
    let recovery_id = sig_ref[64];

    let message_hash = keccak256(message.as_ref());

    let result = recover(&message_hash, r_s_signature, recovery_id.into());
    result.map(|v| v == address).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secp256k1::SecretKey;
    use std::str::FromStr;

    fn get_key_address(key: impl Key) -> Address {
        key.address()
    }

    #[test]
    fn sign_then_verify() {
        let message = "Hello, world!";
        let address = Address::from_str("0x01E29630AF0bAC5f3f28B97040b4f59ab47584F7").unwrap();

        let key =
            SecretKey::from_str("46886194468bb6e0faa36c12cebb6f0ca104ddbc8ec9d39246d718eba6e22d67")
                .unwrap();

        // Ensure that the address belongs to the key.
        assert_eq!(address, get_key_address(&key));

        let sig = sign(message, &key);

        // Verify message signature by address.
        assert!(verify(message, address, sig.signature.0));
    }
}
