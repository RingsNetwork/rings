//! The frame header: the one part of the wire every version decodes alike (#1104).
//!
//! ```text
//!   frame        = MAGIC ‖ wire_version ‖ rest(wire_version)
//!   MAGIC        = "RINGS\0"                      fixed forever
//!   wire_version = u16, big-endian                fixed forever, read before any codec
//!   rest(1)      = kind ‖ codec(body)             kind: 0 payload, 1 link control
//! ```
//!
//! Law (frame header): every version reads `MAGIC ‖ wire_version` the same way, and decodes
//! `rest` only when the version is its own. A frame of another version is refused before its
//! body is decoded, with [`Error::IncompatibleWireVersion`] naming both versions. Frames from
//! before wire versioning opened with `RINGS-PAYLOAD\0` or `RINGS-LINK\0`: they share the first
//! five bytes of `MAGIC` and differ in the sixth (`-` against `\0`), so such a frame is told
//! apart and named as from a peer before wire versioning, never read as a version.
//!
//! Both join paths carry the header, since both carry frames: the HTTP handshake exchanges
//! base58-check frames, and the overlay handshake travels in link frames. Every link frame
//! carries it as well, so two nodes of different wire versions never exchange a frame, and a
//! connected overlay holds one wire version.
//!
//! Bump rule: [`WIRE_VERSION`] goes up by exactly one per release, relative to the last
//! released wire version, when the release changes anything a node of that version would
//! reject or misread: a frame, a message, a transport signal, a signature domain, a handshake
//! rule. Changes between two releases share the one bump; a change no peer can observe never
//! bumps it, and the crate version moves independently. Every bump is a total cutover, as
//! every wire change before 1.0 is: no build decodes the `rest` of another version, and
//! nothing is negotiated.

use std::fmt;

use bytes::Bytes;

use crate::error::Error;
use crate::error::Result;

/// The wire version of this build; see the bump rule.
pub const WIRE_VERSION: u16 = 2;

/// The opening of every frame, fixed forever (the frame-header law).
const FRAME_MAGIC: &[u8] = b"RINGS\0";
/// The opening every frame from before wire versioning shares: its markers were
/// `RINGS-PAYLOAD\0` and `RINGS-LINK\0`.
const PRE_VERSIONING_OPENING: &[u8] = b"RINGS-";
/// The length of a wire version on the wire.
const WIRE_VERSION_LEN: usize = 2;

/// The bytes a frame of this wire version spends before its body: the magic, the version, and
/// the kind.
pub(super) const FRAME_HEADER_LEN: usize = FRAME_MAGIC.len() + WIRE_VERSION_LEN + 1;

/// What a frame of this wire version carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameKind {
    /// A message payload.
    Payload,
    /// A link-control frame.
    LinkControl,
}

impl FrameKind {
    /// The byte that names this kind on the wire.
    const fn byte(self) -> u8 {
        match self {
            Self::Payload => 0,
            Self::LinkControl => 1,
        }
    }

    /// The kind `byte` names, if any.
    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Payload),
            1 => Some(Self::LinkControl),
            _ => None,
        }
    }
}

/// A peer's wire version, as its frame or its handshake endpoint states it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerWireVersion {
    /// The versioned wire, at this version.
    Versioned(u16),
    /// A peer from before wire versioning, whose frames carry no version.
    BeforeVersioning,
}

impl PeerWireVersion {
    /// The peer's version number; `None` for a peer from before wire versioning.
    pub const fn number(self) -> Option<u16> {
        match self {
            Self::Versioned(version) => Some(version),
            Self::BeforeVersioning => None,
        }
    }

    /// The peer's wire version as an endpoint reports it: a number, or none for a peer from
    /// before wire versioning.
    pub const fn reported(number: Option<u16>) -> Self {
        match number {
            Some(version) => Self::Versioned(version),
            None => Self::BeforeVersioning,
        }
    }

    /// Admit a peer of this version: `Ok` iff it is [`WIRE_VERSION`], and otherwise
    /// [`Self::mismatch`].
    pub fn admit(self) -> std::result::Result<(), WireVersionMismatch> {
        if self == Self::Versioned(WIRE_VERSION) {
            Ok(())
        } else {
            Err(self.mismatch())
        }
    }

    /// This build's wire version against this one.
    pub const fn mismatch(self) -> WireVersionMismatch {
        WireVersionMismatch {
            ours: WIRE_VERSION,
            theirs: self,
        }
    }
}

impl fmt::Display for PeerWireVersion {
    /// The version number, or the words for a peer from before wire versioning.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Versioned(version) => write!(f, "{version}"),
            Self::BeforeVersioning => f.write_str("from before wire versioning"),
        }
    }
}

/// Two wire versions that do not meet: this build's and a peer's (#1104).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireVersionMismatch {
    /// This build's wire version.
    pub ours: u16,
    /// The peer's, as its frame or its handshake endpoint states it.
    pub theirs: PeerWireVersion,
}

impl fmt::Display for WireVersionMismatch {
    /// Both versions, ours first.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ours is {}, the peer's is {}", self.ours, self.theirs)
    }
}

impl From<WireVersionMismatch> for Error {
    /// The core's refusal of a peer of another wire version.
    fn from(mismatch: WireVersionMismatch) -> Self {
        Self::IncompatibleWireVersion(mismatch)
    }
}

/// The frame `MAGIC ‖ WIRE_VERSION ‖ kind ‖ body`, with the length sum checked.
pub(super) fn frame_bytes(kind: FrameKind, body: &[u8]) -> Result<Bytes> {
    let capacity = FRAME_HEADER_LEN
        .checked_add(body.len())
        .ok_or(Error::MessageSizeOverflow)?;
    let mut wire = Vec::with_capacity(capacity);
    wire.extend_from_slice(FRAME_MAGIC);
    wire.extend_from_slice(&WIRE_VERSION.to_be_bytes());
    wire.push(kind.byte());
    wire.extend_from_slice(body);
    Ok(Bytes::from(wire))
}

/// Read the header of `bytes` (the frame-header law): the kind and body of a frame of this wire
/// version, and otherwise the refusal.
///
/// ```text
///   bytes ─┬─ MAGIC ‖ WIRE_VERSION ‖ kind ‖ body ─▶ (kind, body)
///          ├─ MAGIC ‖ v ≠ WIRE_VERSION ‖ …     ─▶ IncompatibleWireVersion { theirs: Versioned(v) }
///          ├─ "RINGS-" …                       ─▶ IncompatibleWireVersion { theirs: BeforeVersioning }
///          └─ otherwise, or an unknown kind     ─▶ UnmarkedFrame
/// ```
pub(super) fn split_frame(bytes: &[u8]) -> Result<(FrameKind, &[u8])> {
    let Some(versioned) = bytes.strip_prefix(FRAME_MAGIC) else {
        return Err(if bytes.starts_with(PRE_VERSIONING_OPENING) {
            PeerWireVersion::BeforeVersioning.mismatch().into()
        } else {
            Error::UnmarkedFrame
        });
    };
    let Some((version, rest)) = versioned.split_first_chunk::<WIRE_VERSION_LEN>() else {
        return Err(Error::UnmarkedFrame);
    };
    PeerWireVersion::Versioned(u16::from_be_bytes(*version)).admit()?;
    let Some((kind, body)) = rest.split_first() else {
        return Err(Error::UnmarkedFrame);
    };
    let kind = FrameKind::from_byte(*kind).ok_or(Error::UnmarkedFrame)?;
    Ok((kind, body))
}
