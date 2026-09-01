// SPDX-License-Identifier: MIT

use std::{fmt, io, mem::size_of, num::NonZeroI32};

use crate::{emit_i32, parse_i32, Emitable, Field, Parseable, Rest};

const CODE: Field = 0..4;
const PAYLOAD: Rest = 4..;
const ERROR_HEADER_LEN: usize = PAYLOAD.start;

pub trait ErrorContext<T: std::fmt::Display> {
    fn context(self, msg: T) -> Self;
}

#[derive(Debug)]
pub struct DecodeError {
    msg: String,
}

impl<T: std::fmt::Display> ErrorContext<T> for DecodeError {
    fn context(self, msg: T) -> Self {
        Self {
            msg: format!("{} caused by {}", msg, self.msg),
        }
    }
}

impl<T, M> ErrorContext<M> for Result<T, DecodeError>
where
    M: std::fmt::Display,
{
    fn context(self, msg: M) -> Result<T, DecodeError> {
        match self {
            Ok(t) => Ok(t),
            Err(e) => Err(e.context(msg)),
        }
    }
}

impl From<&str> for DecodeError {
    fn from(msg: &str) -> Self {
        Self {
            msg: msg.to_string(),
        }
    }
}

impl From<String> for DecodeError {
    fn from(msg: String) -> Self {
        Self { msg }
    }
}

impl From<std::string::FromUtf8Error> for DecodeError {
    fn from(err: std::string::FromUtf8Error) -> Self {
        Self {
            msg: format!("Invalid UTF-8 sequence: {}", err),
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for DecodeError {}

impl DecodeError {
    pub fn invalid_buffer(
        name: &str,
        received: usize,
        minimum_length: usize,
    ) -> Self {
        Self {
            msg: format!(
                "Invalid buffer {name}. Expected at least {minimum_length} \
                 bytes, received {received} bytes"
            ),
        }
    }
    pub fn invalid_mac_address(received: usize) -> Self {
        Self {
            msg: format!(
                "Invalid MAC address. Expected 6 bytes, received {received} \
                 bytes"
            ),
        }
    }

    pub fn invalid_ip_address(received: usize) -> Self {
        Self {
            msg: format!(
                "Invalid IP address. Expected 4 or 16 bytes, received \
                 {received} bytes"
            ),
        }
    }

    pub fn invalid_number(expected: usize, received: usize) -> Self {
        Self {
            msg: format!(
                "Invalid number. Expected {expected} bytes, received \
                 {received} bytes"
            ),
        }
    }

    pub fn nla_buffer_too_small(buffer_len: usize, nla_len: usize) -> Self {
        Self {
            msg: format!(
                "buffer has length {buffer_len}, but an NLA header is \
                 {nla_len} bytes"
            ),
        }
    }

    pub fn nla_length_mismatch(buffer_len: usize, nla_len: usize) -> Self {
        Self {
            msg: format!(
                "buffer has length: {buffer_len}, but the NLA is {nla_len} \
                 bytes"
            ),
        }
    }

    pub fn nla_invalid_length(buffer_len: usize, nla_len: usize) -> Self {
        Self {
            msg: format!(
                "NLA has invalid length: {nla_len} (should be at least \
                 {buffer_len} bytes)"
            ),
        }
    }

    pub fn buffer_too_small(buffer_len: usize, value_len: usize) -> Self {
        Self {
            msg: format!(
                "Buffer too small: {buffer_len} (should be at least \
                 {value_len} bytes"
            ),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub struct ErrorBuffer<T> {
    buffer: T,
}

impl<T: AsRef<[u8]>> ErrorBuffer<T> {
    pub fn new(buffer: T) -> ErrorBuffer<T> {
        ErrorBuffer { buffer }
    }

    /// Consume the packet, returning the underlying buffer.
    pub fn into_inner(self) -> T {
        self.buffer
    }

    pub fn new_checked(buffer: T) -> Result<Self, DecodeError> {
        let packet = Self::new(buffer);
        packet
            .check_buffer_length()
            .context("invalid ErrorBuffer length")?;
        Ok(packet)
    }

    fn check_buffer_length(&self) -> Result<(), DecodeError> {
        let len = self.buffer.as_ref().len();
        if len < ERROR_HEADER_LEN {
            Err(DecodeError {
                msg: format!(
                    "invalid ErrorBuffer: length is {len} but ErrorBuffer are \
                     at least {ERROR_HEADER_LEN} bytes"
                ),
            })
        } else {
            Ok(())
        }
    }

    /// Return the error code.
    ///
    /// Returns `None` when there is no error to report (the message is an ACK),
    /// or a `Some(e)` if there is a non-zero error code `e` to report (the
    /// message is a NACK).
    pub fn code(&self) -> Option<NonZeroI32> {
        let data = self.buffer.as_ref();
        NonZeroI32::new(parse_i32(&data[CODE]).unwrap())
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> ErrorBuffer<&'a T> {
    /// Return a pointer to the payload.
    pub fn payload(&self) -> &'a [u8] {
        let data = self.buffer.as_ref();
        &data[PAYLOAD]
    }
}

impl<T: AsRef<[u8]> + AsMut<[u8]> + ?Sized> ErrorBuffer<&mut T> {
    /// Return a mutable pointer to the payload.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        let data = self.buffer.as_mut();
        &mut data[PAYLOAD]
    }
}

impl<T: AsRef<[u8]> + AsMut<[u8]>> ErrorBuffer<T> {
    /// set the error code field
    pub fn set_code(&mut self, value: i32) {
        let data = self.buffer.as_mut();
        emit_i32(&mut data[CODE], value).unwrap();
    }
}

/// An `NLMSG_ERROR` message.
///
/// Per [RFC 3549 section 2.3.2.2], this message carries the return code for a
/// request which will indicate either success (an ACK) or failure (a NACK).
///
/// [RFC 3549 section 2.3.2.2]: https://datatracker.ietf.org/doc/html/rfc3549#section-2.3.2.2
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ErrorMessage {
    /// The error code.
    ///
    /// Holds `None` when there is no error to report (the message is an ACK),
    /// or a `Some(e)` if there is a non-zero error code `e` to report (the
    /// message is a NACK).
    ///
    /// See [Netlink message types] for details.
    ///
    /// [Netlink message types]: https://kernel.org/doc/html/next/userspace-api/netlink/intro.html#netlink-message-types
    pub code: Option<NonZeroI32>,
    /// The original request's header.
    pub header: Vec<u8>,
}

impl Emitable for ErrorMessage {
    fn buffer_len(&self) -> usize {
        size_of::<i32>() + self.header.len()
    }
    fn emit(&self, buffer: &mut [u8]) {
        let mut buffer = ErrorBuffer::new(buffer);
        buffer.set_code(self.raw_code());
        buffer.payload_mut().copy_from_slice(&self.header)
    }
}

impl<T: AsRef<[u8]>> Parseable<ErrorBuffer<&T>> for ErrorMessage {
    fn parse(buf: &ErrorBuffer<&T>) -> Result<ErrorMessage, DecodeError> {
        // FIXME: The payload of an error is basically a truncated packet, which
        // requires custom logic to parse correctly. For now we just
        // return it as a Vec<u8> let header: NetlinkHeader = {
        //     NetlinkBuffer::new_checked(self.payload())
        //         .context("failed to parse netlink header")?
        //         .parse()
        //         .context("failed to parse nelink header")?
        // };
        Ok(ErrorMessage {
            code: buf.code(),
            header: buf.payload().to_vec(),
        })
    }
}

impl ErrorMessage {
    /// Returns the raw error code.
    pub fn raw_code(&self) -> i32 {
        self.code.map_or(0, NonZeroI32::get)
    }

    /// According to [`netlink(7)`](https://linux.die.net/man/7/netlink)
    /// the `NLMSG_ERROR` return Negative errno or 0 for acknowledgements.
    ///
    /// convert into [`std::io::Error`](https://doc.rust-lang.org/std/io/struct.Error.html)
    /// using the absolute value from errno code
    pub fn to_io(&self) -> io::Error {
        io::Error::from_raw_os_error(self.raw_code().abs())
    }
}

impl fmt::Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.to_io(), f)
    }
}

impl From<ErrorMessage> for io::Error {
    fn from(e: ErrorMessage) -> io::Error {
        e.to_io()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_io_error() {
        let io_err = io::Error::from_raw_os_error(95);
        let err_msg = ErrorMessage {
            code: NonZeroI32::new(-95),
            header: vec![],
        };

        let to_io: io::Error = err_msg.to_io();

        assert_eq!(err_msg.to_string(), io_err.to_string());
        assert_eq!(to_io.raw_os_error(), io_err.raw_os_error());
    }

    #[test]
    fn parse_ack() {
        let bytes = vec![0, 0, 0, 0];
        let msg = ErrorBuffer::new_checked(&bytes)
            .and_then(|buf| ErrorMessage::parse(&buf))
            .expect("failed to parse NLMSG_ERROR");
        assert_eq!(
            ErrorMessage {
                code: None,
                header: Vec::new()
            },
            msg
        );
        assert_eq!(msg.raw_code(), 0);
    }

    #[test]
    fn parse_nack() {
        // SAFETY: value is non-zero.
        const ERROR_CODE: NonZeroI32 = NonZeroI32::new(-1234).unwrap();
        let mut bytes = vec![0, 0, 0, 0];
        emit_i32(&mut bytes, ERROR_CODE.get()).unwrap();
        let msg = ErrorBuffer::new_checked(&bytes)
            .and_then(|buf| ErrorMessage::parse(&buf))
            .expect("failed to parse NLMSG_ERROR");
        assert_eq!(
            ErrorMessage {
                code: Some(ERROR_CODE),
                header: Vec::new()
            },
            msg
        );
        assert_eq!(msg.raw_code(), ERROR_CODE.get());
    }
}
