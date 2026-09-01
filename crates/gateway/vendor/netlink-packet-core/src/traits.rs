// SPDX-License-Identifier: MIT

use crate::{DecodeError, NetlinkHeader};

/// A `NetlinkDeserializable` type can be deserialized from a buffer
pub trait NetlinkDeserializable: Sized {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Deserialize the given buffer into `Self`.
    fn deserialize(
        header: &NetlinkHeader,
        payload: &[u8],
    ) -> Result<Self, Self::Error>;
}

pub trait NetlinkSerializable {
    fn message_type(&self) -> u16;

    /// Return the length of the serialized data.
    ///
    /// Most netlink messages are encoded following a
    /// [TLV](https://en.wikipedia.org/wiki/Type-length-value) scheme
    /// and this library takes advantage of this by pre-allocating
    /// buffers of the appropriate size when serializing messages,
    /// which is why `buffer_len` is needed.
    fn buffer_len(&self) -> usize;

    /// Serialize this types and write the serialized data into the given
    /// buffer. `buffer`'s length is exactly `InnerMessage::buffer_len()`.
    /// It means that if `InnerMessage::buffer_len()` is buggy and does not
    /// return the appropriate length, bad things can happen:
    ///
    /// - if `buffer_len()` returns a value _smaller than the actual data_,
    ///   `emit()` may panics
    /// - if `buffer_len()` returns a value _bigger than the actual data_, the
    ///   buffer will contain garbage
    ///
    /// # Panic
    ///
    /// This method panics if the buffer is not big enough.
    fn serialize(&self, buffer: &mut [u8]);
}

/// A type that implements `Emitable` can be serialized.
pub trait Emitable {
    /// Return the length of the serialized data.
    fn buffer_len(&self) -> usize;

    /// Serialize this types and write the serialized data into the given
    /// buffer.
    ///
    /// # Panic
    ///
    /// This method panic if the buffer is not big enough. You **must** make
    /// sure the buffer is big enough before calling this method. You can
    /// use [`buffer_len()`](trait.Emitable.html#method.buffer_len) to check
    /// how big the storage needs to be.
    fn emit(&self, buffer: &mut [u8]);
}

/// A `Parseable` type can be used to deserialize data from the type `T` for
/// which it is implemented.
pub trait Parseable<T>
where
    Self: Sized,
    T: ?Sized,
{
    /// Deserialize the current type.
    fn parse(buf: &T) -> Result<Self, DecodeError>;
}

/// A `Parseable` type can be used to deserialize data from the type `T` for
/// which it is implemented.
pub trait ParseableParametrized<T, P>
where
    Self: Sized,
    T: ?Sized,
{
    /// Deserialize the current type.
    fn parse_with_param(buf: &T, params: P) -> Result<Self, DecodeError>;
}
