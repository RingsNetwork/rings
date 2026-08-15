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
}
