use base58_monero::base58::CHECKSUM_SIZE;
use base58_monero::Error;
use tiny_keccak::Hasher;
use tiny_keccak::Keccak;

/// Decode base58-check without exposing the upstream short-input panic.
pub(crate) fn decode(value: &str) -> Result<Vec<u8>, Error> {
    let bytes = base58_monero::decode(value)?;
    let payload_len = bytes
        .len()
        .checked_sub(CHECKSUM_SIZE)
        .ok_or(Error::InvalidChecksum)?;
    let payload = bytes.get(..payload_len).ok_or(Error::InvalidChecksum)?;
    let checksum = bytes.get(payload_len..).ok_or(Error::InvalidChecksum)?;

    let mut expected = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(payload);
    hasher.finalize(&mut expected);

    if expected.get(..CHECKSUM_SIZE) == Some(checksum) {
        Ok(payload.to_vec())
    } else {
        Err(Error::InvalidChecksum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_inputs_shorter_than_checksum_are_rejected() {
        for len in 0..CHECKSUM_SIZE {
            let encoded = base58_monero::encode(&vec![0u8; len]).unwrap();
            assert!(decode(&encoded).is_err(), "decoded length {len}");
        }
    }

    #[test]
    fn valid_checked_inputs_round_trip() {
        for payload in [&[][..], &[0][..], b"rings"] {
            let encoded = base58_monero::encode_check(payload).unwrap();
            assert_eq!(decode(&encoded).unwrap(), payload);
        }
    }

    #[test]
    fn invalid_checksum_is_rejected() {
        let encoded = base58_monero::encode(&[0u8; CHECKSUM_SIZE]).unwrap();
        assert!(decode(&encoded).is_err());
    }
}
