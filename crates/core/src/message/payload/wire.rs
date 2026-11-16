//! The frames one link carries, and the delegation references inside them.
//!
//! A [`MessagePayload`] is self-contained: each of its two proofs embeds the full [`Delegation`]
//! that signed it. On a link that is redundant, because the same few delegations sign every
//! frame for the lifetime of the connection. The wire form therefore carries, in each session
//! slot, a [`DelegationRef`]:
//!
//! ```text
//!   DelegationRef = Inline(Delegation) + Digest(DelegationDigest)
//!
//!   view    : MessagePayload × PerSlot<DelegationRef> → WirePayload      (borrowing, no copy)
//!   resolve : WirePayload × (DelegationRef → E + Delegation) → E + MessagePayload   (by move)
//! ```
//!
//! Law (round trip): for every `p` and every choice of references `r` with
//! `ρ(r_slot) = p.session(slot)`, `resolve(decode(encode(view(p, r))), ρ) = p`. A digest is the
//! content address of the session it replaces, so the resolved payload is the value that would
//! have travelled inline: what is verified, attributed, and digested does not depend on how a
//! slot was encoded. Neither signature covers the delegation slot (both sign the transaction
//! hash), so re-encoding a slot per link leaves the origin's signature intact.
//!
//! A frame is either a payload or a [`LinkControl`]; the two are told apart by the kind in the
//! frame header (`frame`), read with the wire version before any decoding. A control frame is
//! unsigned: it is meaningful only on the authenticated edge it
//! arrives on, and a digest names a value rather than asserting one. The link is treated as a
//! datagram link: control frames and payload frames may arrive in any order or not at all, and
//! every control frame is idempotent, so a duplicate changes nothing: a stale `Known` never
//! shortens the lease held, and a stale `Unknown` costs only inline frames until the next
//! lease (see the laws of `swarm::session_link`).

use std::borrow::Cow;

use bytes::Bytes;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use serde::Deserialize;
use serde::Serialize;

use super::frame::frame_bytes;
use super::frame::split_frame;
use super::frame::FrameKind;
use super::frame::FRAME_HEADER_LEN;
use super::MessagePayload;
use super::Transaction;
use crate::delegation::Delegation;
use crate::delegation::DelegationDigest;
use crate::delegation::DelegatorAuthorization;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::protocols::MessageRelay;
use crate::message::protocols::MessageVerification;
use crate::message::MessageCategory;
use crate::message::MessageKind;

/// Hard upper bound on the uncompressed `WirePayload` body of a
/// [`FrameKind::CompressedPayload`] frame (#1280).
///
/// Decompression allocates at most this ceiling before reading untrusted LZ4 tokens
/// (`lz4_flex::block::decompress`), defending against decompression bombs without relying on an
/// attacker-controlled size prefix.
pub(crate) const MAX_COMPRESSED_PAYLOAD_BYTES: usize = MAX_DATA_CHANNEL_MESSAGE_SIZE;

/// Upper bound on the uncompressed wire length (`FRAME_HEADER_LEN + body`) of a
/// [`FrameKind::CompressedPayload`] frame.
pub(crate) const MAX_COMPRESSED_FRAME_WIRE_BYTES: usize =
    FRAME_HEADER_LEN + MAX_COMPRESSED_PAYLOAD_BYTES;

/// Pre-shared compile-time protocol dictionary for [`FrameKind::CompressedPayload`] (#1282).
///
/// Contains canonical WebRTC SDP boilerplate lines, ICE candidate tokens, and Rings protocol
/// strings so even the first occurrence inside a small `DhtControl` or `Storage` frame matches
/// against the static dictionary window.
pub(crate) const WIRE_COMPRESSION_DICT: &[u8] = concat!(
    "v=0\r\no=- ",
    " 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n",
    "a=group:BUNDLE 0\r\na=msid-semantic: WMS\r\n",
    "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\n",
    "a=ice-ufrag:",
    "\r\na=ice-pwd:",
    "\r\na=ice-options:trickle\r\n",
    "a=fingerprint:sha-256 ",
    "\r\na=setup:actpass\r\na=setup:active\r\na=setup:passive\r\n",
    "a=mid:0\r\na=sctp-port:5000\r\na=max-message-size:65536\r\n",
    "a=candidate:",
    " 1 udp 2130706431 ",
    " 1 tcp 1518280447 ",
    " typ host generation 0\r\n",
    " typ srflx raddr 0.0.0.0 rport 0 generation 0\r\n",
    " typ relay raddr 0.0.0.0 rport 0 generation 0\r\n",
    "rings-delegation-v1\nsecp256k1\n0x",
    "rings-delegation-v1\ned25519\n0x",
    "rings-core:message-verification:transaction",
    "rings-core:message-verification:payload",
    "https://",
    "wss://",
    "\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
)
.as_bytes();

/// The two delegation slots of a payload, as a product: `PerSlot<T> ≅ T × T`.
///
/// Every per-slot quantity (a reference, an encoding, a room) is carried in this
/// one shape, so the slots are always handled together and in the same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PerSlot<T> {
    /// The slot of the transaction proof: the origin's session.
    pub(crate) origin: T,
    /// The slot of the payload proof: the current hop's session.
    pub(crate) hop: T,
}

impl<T> PerSlot<T> {
    /// The functor action: `map f (a, b) = (f a, f b)`, origin first.
    pub(crate) fn map<U>(self, mut f: impl FnMut(T) -> U) -> PerSlot<U> {
        let origin = f(self.origin);
        let hop = f(self.hop);
        PerSlot { origin, hop }
    }

    /// The slots paired pointwise: `(a, b) × (c, d) ↦ ((a, c), (b, d))`.
    pub(crate) fn zip<U>(self, other: PerSlot<U>) -> PerSlot<(T, U)> {
        PerSlot {
            origin: (self.origin, other.origin),
            hop: (self.hop, other.hop),
        }
    }

    /// Both slots in order, origin first.
    pub(crate) fn into_array(self) -> [T; 2] {
        [self.origin, self.hop]
    }
}

impl PerSlot<DelegatorAuthorization> {
    /// Both slots verified in full: a frame that arrived on no link, whose delegations no lessor
    /// table has seen.
    pub(crate) const fn full_verification() -> Self {
        Self {
            origin: DelegatorAuthorization::Verify,
            hop: DelegatorAuthorization::Verify,
        }
    }
}

impl<T: PartialEq> PerSlot<T> {
    /// Whether both slots hold the same value: the image of the diagonal `T → T × T`, as on a
    /// frame whose origin is its own hop.
    pub(crate) fn is_diagonal(&self) -> bool {
        self.origin == self.hop
    }
}

/// What a delegation slot holds on the wire.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum DelegationRef<'a> {
    /// The delegation itself: the receiver learns it from this frame.
    Inline(Cow<'a, Delegation>),
    /// The content address of a session this link already carried inline.
    Digest(DelegationDigest),
}

impl DelegationRef<'_> {
    /// `session` inline, borrowed for the frame being built.
    pub(crate) const fn inline(session: &Delegation) -> DelegationRef<'_> {
        DelegationRef::Inline(Cow::Borrowed(session))
    }
}

/// How a slot travelled: the shape of a [`DelegationRef`] without its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotEncoding {
    /// The session was carried inline.
    Inline,
    /// The session was carried as its digest.
    Referenced,
}

impl SlotEncoding {
    /// How a receiver treats the delegator authorization of a slot that travelled this way.
    ///
    /// Law (Leased authority): a referenced slot resolves only against the receiver's lessor
    /// table of the link, and every entry of that table was leased by `admit_verified` from a
    /// frame that verified, the delegator's signature over that exact delegation included. So a
    /// referenced slot's delegator authorization is established, and only its expiry and the
    /// delegatee's signature remain to check (#1089). An inline slot is verified in full.
    pub(crate) const fn delegator_authorization(self) -> DelegatorAuthorization {
        match self {
            Self::Inline => DelegatorAuthorization::Verify,
            Self::Referenced => DelegatorAuthorization::Established,
        }
    }
}

impl DelegationRef<'_> {
    /// How this slot travelled.
    pub(crate) const fn encoding(&self) -> SlotEncoding {
        match self {
            Self::Inline(_) => SlotEncoding::Inline,
            Self::Digest(_) => SlotEncoding::Referenced,
        }
    }

    /// The digest this slot references, if it travelled by reference.
    pub(crate) const fn digest(&self) -> Option<DelegationDigest> {
        match self {
            Self::Inline(_) => None,
            Self::Digest(digest) => Some(*digest),
        }
    }
}

/// The wire form of a [`MessageVerification`]: the proof with its delegation slot as a reference.
#[derive(Deserialize, Serialize)]
struct WireVerification<'a> {
    /// The delegation slot.
    delegation: DelegationRef<'a>,
    /// The proof's lifetime stamp, in the clear whether or not the slot is resolved.
    ttl_ms: u64,
    /// The proof's creation stamp.
    ts_ms: u128,
    /// The delegatee signature.
    sig: Cow<'a, [u8]>,
}

/// The wire form of a [`Transaction`].
#[derive(Deserialize, Serialize)]
struct WireTransaction<'a> {
    /// See [`Transaction::destination`].
    destination: Did,
    /// See [`Transaction::tx_id`].
    tx_id: uuid::Uuid,
    /// See [`Transaction::sequence`].
    sequence: u64,
    /// See [`Transaction::reply_via`].
    reply_via: Option<Did>,
    /// See [`Transaction::data`].
    data: Cow<'a, [u8]>,
    /// The origin's proof.
    verification: WireVerification<'a>,
}

/// The wire form of a [`MessagePayload`]: each delegation slot a [`DelegationRef`]. Borrowed when
/// built by [`Self::view`], owned when decoded.
#[derive(Deserialize, Serialize)]
pub(crate) struct WirePayload<'a> {
    /// The origin's transaction.
    transaction: WireTransaction<'a>,
    /// The relay carrier.
    relay: Cow<'a, MessageRelay>,
    /// The current hop's proof.
    verification: WireVerification<'a>,
}

impl<'a> WireVerification<'a> {
    /// View `verification` with `session` in its slot.
    fn view(verification: &'a MessageVerification, delegation: DelegationRef<'a>) -> Self {
        Self {
            delegation,
            ttl_ms: verification.ttl_ms,
            ts_ms: verification.ts_ms,
            sig: Cow::Borrowed(verification.sig.as_slice()),
        }
    }

    /// The self-contained proof, with its slot resolved by `resolve`.
    fn resolve<E>(
        self,
        resolve: &mut impl FnMut(DelegationRef<'a>) -> std::result::Result<Delegation, E>,
    ) -> std::result::Result<MessageVerification, E> {
        let Self {
            delegation,
            ttl_ms,
            ts_ms,
            sig,
        } = self;
        Ok(MessageVerification {
            delegation: resolve(delegation)?,
            ttl_ms,
            ts_ms,
            sig: sig.into_owned(),
        })
    }
}

impl<'a> WirePayload<'a> {
    /// View `payload` with `sessions` in its slots. Nothing is copied: every field borrows.
    ///
    /// Pre: each reference stands for the session in its slot (it is that session inline, or
    /// its digest); the round-trip law holds only then.
    pub(crate) fn view(payload: &'a MessagePayload, sessions: PerSlot<DelegationRef<'a>>) -> Self {
        let transaction = &payload.transaction;
        Self {
            transaction: WireTransaction {
                destination: transaction.destination,
                tx_id: transaction.tx_id,
                sequence: transaction.sequence,
                reply_via: transaction.reply_via,
                data: Cow::Borrowed(transaction.data.as_slice()),
                verification: WireVerification::view(&transaction.verification, sessions.origin),
            },
            relay: Cow::Borrowed(&payload.relay),
            verification: WireVerification::view(&payload.verification, sessions.hop),
        }
    }

    /// View `payload` with both sessions inline: the self-contained frame, valid on any link and
    /// in any context that has no link at all.
    pub(crate) fn inline(payload: &'a MessagePayload) -> Self {
        Self::view(payload, payload.delegations().map(DelegationRef::inline))
    }

    /// View `payload` with each slot travelling as `encoding` says: the session inline, or its
    /// digest. `encoded(p, (Inline, Inline)) = inline(p)`.
    #[cfg(test)]
    pub(crate) fn encoded(
        payload: &'a MessagePayload,
        encoding: PerSlot<SlotEncoding>,
    ) -> Result<Self> {
        let slot = |(session, encoding): (&'a Delegation, SlotEncoding)| match encoding {
            SlotEncoding::Inline => Ok(DelegationRef::inline(session)),
            SlotEncoding::Referenced => session.digest().map(DelegationRef::Digest),
        };
        let slots = payload.delegations().zip(encoding);
        let sessions = PerSlot {
            origin: slot(slots.origin)?,
            hop: slot(slots.hop)?,
        };
        Ok(Self::view(payload, sessions))
    }

    /// The payload, with every slot required inline: the self-contained reading, valid outside
    /// any link. A slot sent by reference is refused as
    /// [`Error::DelegationReferenceUnresolved`].
    pub(crate) fn into_self_contained(self) -> Result<MessagePayload> {
        self.resolve(resolve_inline)
    }

    /// Whether this payload's transaction belongs to a public, cleartext-signed traffic class
    /// eligible for outer wire-frame compression (`MessageCategory::DhtControl` or
    /// `MessageCategory::Storage`).
    ///
    /// Law (compression boundary, #1280): `MessageCategory::E2e` and
    /// `MessageCategory::Application` (including fixed-width Onion frames) never compress on the
    /// wire and are rejected if received as [`FrameKind::CompressedPayload`].
    pub(crate) fn permits_wire_compression(&self) -> bool {
        MessageKind::from_wire(self.transaction.data.as_ref()).is_ok_and(|kind| {
            matches!(
                kind.class(),
                MessageCategory::DhtControl | MessageCategory::Storage
            )
        })
    }

    /// The frame bytes: the frame header of a payload (`frame`), then the encoded body.
    ///
    /// When the carried transaction is `MessageCategory::DhtControl` or
    /// `MessageCategory::Storage` and the serialized body fits
    /// [`MAX_COMPRESSED_PAYLOAD_BYTES`], the body is compressed with
    /// `lz4_flex::block::compress` and emitted as [`FrameKind::CompressedPayload`] iff the
    /// compressed representation is strictly smaller than the uncompressed body; otherwise it
    /// travels uncompressed as [`FrameKind::Payload`].
    pub(crate) fn to_wire(&self) -> Result<Bytes> {
        let body = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        if body.len() <= MAX_COMPRESSED_PAYLOAD_BYTES && self.permits_wire_compression() {
            let compressed = lz4_flex::block::compress_with_dict(&body, WIRE_COMPRESSION_DICT);
            if compressed.len() < body.len() {
                return frame_bytes(FrameKind::CompressedPayload, &compressed);
            }
        }
        frame_bytes(FrameKind::Payload, body.as_slice())
    }

    /// The uncompressed length of [`Self::to_wire`], without allocating it.
    pub(crate) fn wire_size(&self) -> Result<usize> {
        let bytes = rings_codec::serialized_size(self).map_err(Error::CodecSerialize)?;
        let body = usize::try_from(bytes).map_err(|_| Error::MessageSizeOverflow)?;
        FRAME_HEADER_LEN
            .checked_add(body)
            .ok_or(Error::MessageSizeOverflow)
    }

    /// What each slot holds.
    pub(crate) const fn delegation_refs(&self) -> PerSlot<&DelegationRef<'a>> {
        PerSlot {
            origin: &self.transaction.verification.delegation,
            hop: &self.verification.delegation,
        }
    }

    /// The encoded message, readable before the slots are resolved.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn transaction_data(&self) -> &[u8] {
        self.transaction.data.as_ref()
    }

    /// The transaction id, readable before the slots are resolved.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) const fn transaction_id(&self) -> uuid::Uuid {
        self.transaction.tx_id
    }

    /// The self-contained payload: `traverse` of `resolve` over the two slots, origin first.
    ///
    /// Owned fields move; nothing is copied for a decoded frame.
    pub(crate) fn resolve<E>(
        self,
        mut resolve: impl FnMut(DelegationRef<'a>) -> std::result::Result<Delegation, E>,
    ) -> std::result::Result<MessagePayload, E> {
        let Self {
            transaction,
            relay,
            verification,
        } = self;
        let origin = transaction.verification.resolve(&mut resolve)?;
        let hop = verification.resolve(&mut resolve)?;
        Ok(MessagePayload {
            transaction: Transaction {
                destination: transaction.destination,
                tx_id: transaction.tx_id,
                sequence: transaction.sequence,
                reply_via: transaction.reply_via,
                data: transaction.data.into_owned(),
                verification: origin,
            },
            relay: relay.into_owned(),
            verification: hop,
        })
    }
}

/// What the two ends of one link tell each other about delegation references. See the module
/// documentation for why these are unsigned.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum LinkControl {
    /// "A frame of yours carried this session inline and it verified: you may reference it for
    /// `remaining_ms` more." A duration, so that no clock is compared with another; see the
    /// lease law of `swarm::session_link`.
    Known {
        /// The session leased.
        digest: DelegationDigest,
        /// What is left of the lease when the end that sent this frame granted or renewed it.
        remaining_ms: u128,
    },
    /// "A frame of yours referenced this digest and I do not hold it: the frame was dropped;
    /// send the session inline."
    Unknown(DelegationDigest),
}

impl LinkControl {
    /// The session this frame is about.
    pub(crate) const fn digest(&self) -> DelegationDigest {
        match self {
            Self::Known { digest, .. } | Self::Unknown(digest) => *digest,
        }
    }

    /// The frame bytes: the frame header of a link-control frame (`frame`), then the encoded
    /// body.
    pub(crate) fn to_wire(&self) -> Result<Bytes> {
        let body = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        frame_bytes(FrameKind::LinkControl, body.as_slice())
    }
}

/// One decoded frame.
pub(crate) enum LinkFrame {
    /// A payload whose delegation slots may still be references.
    Payload(Box<WirePayload<'static>>),
    /// A link-control frame.
    Control(LinkControl),
}

impl LinkFrame {
    /// Decode one frame by its header (the frame-header law of `frame`).
    ///
    /// ```text
    ///   bytes ─┬─ header of this version, payload ────────────▶ Payload(decode body)
    ///          ├─ header of this version, compressed payload ─▶ Payload(decompress & decode body)
    ///          ├─ header of this version, link control ───────▶ Control(decode body)
    ///          ├─ header of another version ──────────────────▶ IncompatibleWireVersion
    ///          └─ otherwise ──────────────────────────────────▶ UnmarkedFrame
    /// ```
    #[cfg(test)]
    pub(crate) fn from_wire(bytes: &[u8]) -> Result<Self> {
        Self::from_wire_with_uncompressed_len(bytes).map(|(frame, _)| frame)
    }

    /// Decode one frame and report its uncompressed wire length (`FRAME_HEADER_LEN` plus the
    /// uncompressed body length), so inbound capacity and reassembly budgets account for the
    /// decompressed representation (#1280).
    pub(crate) fn from_wire_with_uncompressed_len(bytes: &[u8]) -> Result<(Self, usize)> {
        let (kind, body) = split_frame(bytes)?;
        match kind {
            FrameKind::Payload => rings_codec::deserialize(body)
                .map(Box::new)
                .map(|payload| (Self::Payload(payload), bytes.len()))
                .map_err(Error::CodecDeserialize),
            FrameKind::CompressedPayload => {
                if body.is_empty() || body.len() >= MAX_COMPRESSED_PAYLOAD_BYTES {
                    return Err(Error::InvalidMessage(
                        "compressed payload frame body length is out of bounds".to_owned(),
                    ));
                }
                let decompressed = lz4_flex::block::decompress_with_dict(
                    body,
                    MAX_COMPRESSED_PAYLOAD_BYTES,
                    WIRE_COMPRESSION_DICT,
                )
                .map_err(|error| {
                    Error::InvalidMessage(format!(
                        "compressed payload frame failed LZ4 decompression: {error}"
                    ))
                })?;
                if body.len() >= decompressed.len() {
                    return Err(Error::InvalidMessage(
                        "compressed payload frame did not shrink body".to_owned(),
                    ));
                }
                let uncompressed_len = FRAME_HEADER_LEN
                    .checked_add(decompressed.len())
                    .ok_or(Error::MessageSizeOverflow)?;
                let payload: Box<WirePayload<'static>> = rings_codec::deserialize(&decompressed)
                    .map(Box::new)
                    .map_err(Error::CodecDeserialize)?;
                if !payload.permits_wire_compression() {
                    return Err(Error::InvalidMessage(
                        "compressed payload frame carries a message category that forbids compression"
                            .to_owned(),
                    ));
                }
                Ok((Self::Payload(payload), uncompressed_len))
            }
            FrameKind::LinkControl => rings_codec::deserialize(body)
                .map(|control| (Self::Control(control), bytes.len()))
                .map_err(Error::CodecDeserialize),
        }
    }
}

/// Resolve a slot of a frame that travels outside any link: only an inline session resolves.
fn resolve_inline(delegation: DelegationRef<'_>) -> Result<Delegation> {
    match delegation {
        DelegationRef::Inline(session) => Ok(session.into_owned()),
        DelegationRef::Digest(digest) => Err(Error::DelegationReferenceUnresolved(digest)),
    }
}

#[cfg(test)]
#[test]
fn test_only_a_referenced_slot_carries_an_established_authorization() {
    assert_eq!(
        SlotEncoding::Inline.delegator_authorization(),
        DelegatorAuthorization::Verify
    );
    assert_eq!(
        SlotEncoding::Referenced.delegator_authorization(),
        DelegatorAuthorization::Established
    );
}
