#![deny(missing_docs)]

//! Serde wire helpers backed by the Rings postcard codec.

use std::error::Error as StdError;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Codec error returned by Rings wire helper functions.
#[derive(Debug)]
pub enum Error {
    /// Serialization failed.
    Serialize(postcard::Error),
    /// Deserialization failed.
    Deserialize(postcard::Error),
    /// Deserialization succeeded before all bytes were consumed.
    TrailingBytes {
        /// Number of bytes consumed by the decoder.
        decoded: usize,
        /// Total number of bytes provided to the decoder.
        total: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(f, "{error}"),
            Self::Deserialize(error) => write!(f, "{error}"),
            Self::TrailingBytes { decoded, total } => write!(
                f,
                "deserializer consumed {decoded} of {total} bytes; trailing bytes remain"
            ),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Serialize(error) => Some(error),
            Self::Deserialize(error) => Some(error),
            Self::TrailingBytes { .. } => None,
        }
    }
}

impl From<postcard::Error> for Error {
    fn from(error: postcard::Error) -> Self {
        Self::Serialize(error)
    }
}

impl Error {
    fn deserialize(error: postcard::Error) -> Self {
        Self::Deserialize(error)
    }
}

/// Codec result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Serialize a serde value using the Rings wire encoding.
pub fn serialize<T>(value: &T) -> Result<Vec<u8>>
where T: Serialize {
    postcard::to_allocvec(value).map_err(Error::from)
}

/// Deserialize a serde value using the Rings wire encoding.
pub fn deserialize<T>(bytes: &[u8]) -> Result<T>
where T: DeserializeOwned {
    let (value, remaining) = postcard::take_from_bytes(bytes).map_err(Error::deserialize)?;
    match remaining.is_empty() {
        true => Ok(value),
        false => Err(Error::TrailingBytes {
            decoded: bytes.len() - remaining.len(),
            total: bytes.len(),
        }),
    }
}

/// Return the serialized size of a serde value using the default wire encoding.
pub fn serialized_size<T>(value: &T) -> Result<u64>
where T: Serialize {
    serialize(value).map(|bytes| bytes.len() as u64)
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
    fn test_roundtrip_and_size_match() -> std::result::Result<(), Box<dyn StdError>> {
        let value = Example {
            id: 42,
            label: "rings".to_string(),
            bytes: vec![1, 2, 3, 4],
        };

        let encoded = serialize(&value)?;
        assert_eq!(
            encoded,
            vec![42, 5, b'r', b'i', b'n', b'g', b's', 4, 1, 2, 3, 4],
            "wire encoding must stay stable for the Rings codec"
        );
        assert_eq!(u64::try_from(encoded.len())?, serialized_size(&value)?);
        assert_eq!(deserialize::<Example>(&encoded)?, value);
        Ok(())
    }

    #[test]
    fn test_roundtrip_128_bit_integers() -> std::result::Result<(), Box<dyn StdError>> {
        let value = WideIntegers {
            signed: -123_456_789_012_345_678_901_234_567_890i128,
            unsigned: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10u128,
        };

        let encoded = serialize(&value)?;
        assert_eq!(u64::try_from(encoded.len())?, serialized_size(&value)?);
        assert_eq!(deserialize::<WideIntegers>(&encoded)?, value);
        Ok(())
    }

    #[test]
    fn test_deserialize_rejects_trailing_bytes() -> std::result::Result<(), Box<dyn StdError>> {
        let mut encoded = serialize(&42u64)?;
        encoded.extend_from_slice(&[1, 2, 3]);

        match deserialize::<u64>(&encoded) {
            Err(error) => {
                assert!(matches!(error, Error::TrailingBytes {
                    decoded: 1,
                    total: 4
                }));
                Ok(())
            }
            Ok(_) => Err("trailing bytes must fail".into()),
        }
    }
}
