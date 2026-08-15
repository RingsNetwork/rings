//! serde compatibility for wincode, suitable for replacing bincode

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

use core::{marker::PhantomData, mem::MaybeUninit};

use wincode::{
    error::{ReadResult, WriteError, WriteResult},
    io::{Reader, Writer},
};

#[cfg(feature = "alloc")]
extern crate alloc;

mod de;
mod ser;

pub use {de::Deserializer, ser::Serializer, wincode};

/// Wrapper struct that impls [`wincode::SchemaRead`] and
/// [`wincode::SchemaWrite`] for types that impl [`serde::Deserialize`] and
/// [`serde::Serialize`], respectively.
#[repr(transparent)]
pub struct SerdeCompat<T> {
    _marker: PhantomData<T>,
}

unsafe impl<'de, C, T> wincode::SchemaRead<'de, C> for SerdeCompat<T>
where
    C: wincode::config::Config,
    T: serde::Deserialize<'de>,
{
    type Dst = T;

    fn read(
        reader: impl Reader<'de>,
        dst: &mut MaybeUninit<Self::Dst>,
    ) -> ReadResult<()> {
        let deserializer = Deserializer::<_, C>::new(reader);
        let value = T::deserialize(deserializer)?;
        dst.write(value);
        Ok(())
    }
}

unsafe impl<C, T> wincode::SchemaWrite<C> for SerdeCompat<T>
where
    C: wincode::config::Config,
    T: serde::Serialize,
{
    type Src = T;

    fn size_of(src: &Self::Src) -> WriteResult<usize> {
        let mut serializer = ser::SizeOf::<C>::new();
        serializer = src.serialize(serializer)?;
        Ok(serializer.serialized_size())
    }

    fn write(writer: impl Writer, src: &Self::Src) -> WriteResult<()> {
        let serializer = Serializer::<_, C>::new(writer);
        src.serialize(serializer).map_err(WriteError::from)
    }
}

#[cfg(all(feature = "alloc", test))]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::SerdeCompat;

    #[test]
    fn test_flattened_roundtrip()
    -> Result<(), alloc::boxed::Box<dyn core::error::Error>> {
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct InnerMost<'a> {
            #[serde(borrow)]
            msg: &'a str,
            #[serde(borrow)]
            bytes: &'a [u8],
        }
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct InnerMore<'a> {
            u32_value: u32,
            #[serde(borrow)]
            inner: InnerMost<'a>,
        }
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct Outer<'a> {
            #[serde(borrow)]
            inner: InnerMore<'a>,
            bool_value: bool,
        }

        let value = Outer {
            inner: InnerMore {
                u32_value: 69_420,
                inner: InnerMost {
                    msg: "test msg",
                    bytes: b"test bytes",
                },
            },
            bool_value: true,
        };
        let value_serialized_bincode = bincode::serialize(&value)?;
        let value_serialized_wincode =
            <SerdeCompat<Outer> as wincode::Serialize>::serialize(&value)?;
        assert_eq!(value_serialized_bincode, value_serialized_wincode);
        let value_deserialized =
            <SerdeCompat<Outer> as wincode::Deserialize>::deserialize(
                &value_serialized_wincode,
            )?;
        assert_eq!(value_deserialized, value);
        Ok(())
    }
}
