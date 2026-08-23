#![deny(missing_docs)]

//! Serde wire helpers backed by the Rings postcard codec.

use std::error::Error as StdError;
use std::fmt;

use serde::de::DeserializeOwned;
use serde::de::EnumAccess;
use serde::de::Visitor;
use serde::Deserialize;
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

/// Deserialize one value from the start of `bytes` and return the unconsumed suffix.
///
/// Borrowing outputs such as `&[u8]` remain views into the input. This is used
/// for allocation-free inspection of bounded-admission envelope prefixes.
pub fn deserialize_prefix<'de, T>(bytes: &'de [u8]) -> Result<(T, &'de [u8])>
where T: Deserialize<'de> {
    postcard::take_from_bytes(bytes).map_err(Error::deserialize)
}

struct EnumVariant(u32);

impl<'de> Deserialize<'de> for EnumVariant {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        deserializer.deserialize_enum("RingsEnum", &[], EnumVariantVisitor)
    }
}

struct EnumVariantVisitor;

impl<'de> Visitor<'de> for EnumVariantVisitor {
    type Value = EnumVariant;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Rings wire enum discriminant")
    }

    fn visit_enum<A>(self, data: A) -> std::result::Result<Self::Value, A::Error>
    where A: EnumAccess<'de> {
        let (variant, _) = data.variant::<u32>()?;
        Ok(EnumVariant(variant))
    }
}

/// Read an enum variant index without deserializing its body.
///
/// This supports bounded admission decisions that must inspect a Rings postcard
/// envelope before allocating its potentially large variant payload.
pub fn deserialize_enum_variant(bytes: &[u8]) -> Result<u32> {
    let mut deserializer = postcard::Deserializer::from_bytes(bytes);
    EnumVariant::deserialize(&mut deserializer)
        .map(|variant| variant.0)
        .map_err(Error::deserialize)
}

/// Return the serialized size of a serde value using the default wire encoding.
pub fn serialized_size<T>(value: &T) -> Result<u64>
where T: Serialize {
    postcard::experimental::serialized_size(value)
        .map(|bytes| bytes as u64)
        .map_err(Error::from)
}

#[cfg(test)]
mod tests {
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

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    enum TaggedBody {
        Empty,
        Bytes(Vec<u8>),
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
    fn enum_variant_decode_does_not_require_the_variant_body() -> Result<()> {
        assert_eq!(
            deserialize_enum_variant(&serialize(&TaggedBody::Empty)?)?,
            0
        );
        let encoded = serialize(&TaggedBody::Bytes(vec![7; 1024]))?;

        assert_eq!(deserialize_enum_variant(&encoded)?, 1);
        assert!(encoded.starts_with(&[1]));
        let variant_only = [1];
        assert_eq!(deserialize_enum_variant(&variant_only)?, 1);
        assert!(deserialize::<TaggedBody>(&variant_only).is_err());
        Ok(())
    }

    #[test]
    fn prefix_decode_borrows_byte_slices_without_consuming_suffix() -> Result<()> {
        let mut encoded = serialize(&vec![1_u8, 2, 3])?;
        encoded.push(99);

        let (borrowed, remaining) = deserialize_prefix::<&[u8]>(&encoded)?;

        assert_eq!(borrowed, &[1, 2, 3]);
        assert_eq!(remaining, &[99]);
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
