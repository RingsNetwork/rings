#![deny(missing_docs)]

//! Bincode-compatible serde wire helpers backed by maintained crates.

use std::error::Error as StdError;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_wincode::wincode;
use serde_wincode::SerdeCompat;

/// Codec error returned by bincode-compatible helper functions.
#[derive(Debug)]
pub enum Error {
    /// Serialization failed.
    Serialize(wincode::error::WriteError),
    /// Deserialization failed.
    Deserialize(wincode::error::ReadError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(f, "{error}"),
            Self::Deserialize(error) => write!(f, "{error}"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Serialize(error) => Some(error),
            Self::Deserialize(error) => Some(error),
        }
    }
}

impl From<wincode::error::WriteError> for Error {
    fn from(error: wincode::error::WriteError) -> Self {
        Self::Serialize(error)
    }
}

impl From<wincode::error::ReadError> for Error {
    fn from(error: wincode::error::ReadError) -> Self {
        Self::Deserialize(error)
    }
}

/// Codec result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Serialize a serde value using bincode-compatible default wire encoding.
pub fn serialize<T>(value: &T) -> Result<Vec<u8>>
where T: Serialize {
    <SerdeCompat<T> as wincode::Serialize>::serialize(value).map_err(Error::from)
}

/// Deserialize a serde value using bincode-compatible default wire encoding.
pub fn deserialize<T>(bytes: &[u8]) -> Result<T>
where T: DeserializeOwned {
    <SerdeCompat<T> as wincode::Deserialize>::deserialize(bytes).map_err(Error::from)
}

/// Return the serialized size of a serde value using the default wire encoding.
pub fn serialized_size<T>(value: &T) -> Result<u64>
where T: Serialize {
    <SerdeCompat<T> as wincode::Serialize>::serialized_size(value).map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Example {
        id: u64,
        label: String,
        bytes: Vec<u8>,
    }

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct WideIntegers {
        signed: i128,
        unsigned: u128,
    }

    #[test]
    fn roundtrip_and_size_match() -> std::result::Result<(), Box<dyn StdError>> {
        let value = Example {
            id: 42,
            label: "rings".to_string(),
            bytes: vec![1, 2, 3, 4],
        };

        let encoded = serialize(&value)?;
        assert_eq!(
            encoded,
            vec![
                42, 0, 0, 0, 0, 0, 0, 0, // id
                5, 0, 0, 0, 0, 0, 0, 0, b'r', b'i', b'n', b'g', b's', // label
                4, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, // bytes
            ],
            "wire encoding must stay compatible with bincode 1 default options"
        );
        assert_eq!(u64::try_from(encoded.len())?, serialized_size(&value)?);
        assert_eq!(deserialize::<Example>(&encoded)?, value);
        Ok(())
    }

    #[test]
    fn roundtrip_128_bit_integers() -> std::result::Result<(), Box<dyn StdError>> {
        let value = WideIntegers {
            signed: -123_456_789_012_345_678_901_234_567_890i128,
            unsigned: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10u128,
        };

        let encoded = serialize(&value)?;
        let expected = value
            .signed
            .to_le_bytes()
            .into_iter()
            .chain(value.unsigned.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            encoded, expected,
            "128-bit integers must encode as bincode-compatible little-endian bytes"
        );
        assert_eq!(u64::try_from(encoded.len())?, serialized_size(&value)?);
        assert_eq!(deserialize::<WideIntegers>(&encoded)?, value);
        Ok(())
    }
}
