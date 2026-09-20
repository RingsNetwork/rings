//! The frames one link carries, and the session references inside them.
//!
//! A [`MessagePayload`] is self-contained: each of its two proofs embeds the full [`Session`]
//! that signed it. On a link that is redundant, because the same few delegations sign every
//! frame for the lifetime of the connection. The wire form therefore carries, in each session
//! slot, a [`SessionRef`]:
//!
//! ```text
//!   SessionRef = Inline(Session) + Digest(SessionDigest)
//!
//!   view    : MessagePayload × PerSlot<SessionRef> → WirePayload      (borrowing, no copy)
//!   resolve : WirePayload × (SessionRef → E + Session) → E + MessagePayload   (by move)
//! ```
//!
//! Law (round trip): for every `p` and every choice of references `r` with
//! `ρ(r_slot) = p.session(slot)`, `resolve(decode(encode(view(p, r))), ρ) = p`. A digest is the
//! content address of the session it replaces, so the resolved payload is the value that would
//! have travelled inline: what is verified, attributed, and digested does not depend on how a
//! slot was encoded. Neither signature covers the session slot (both sign the transaction
//! hash), so re-encoding a slot per link leaves the origin's signature intact.
//!
//! A frame is either a payload or a [`LinkControl`]; the two are told apart by a marker before
//! any decoding. A control frame is unsigned: it is meaningful only on the authenticated edge it
//! arrives on, an announced session authenticates itself (its account signature), and a digest
//! names a value rather than asserting one. The link is treated as a datagram link: control
//! frames and payload frames may arrive in any order or not at all, and every control frame is
//! idempotent, so a duplicate or a stale one changes nothing.

use std::borrow::Cow;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use super::MessagePayload;
use super::Transaction;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::protocols::MessageRelay;
use crate::message::protocols::MessageVerification;
use crate::message::protocols::ProofLifetime;
use crate::session::Session;
use crate::session::SessionDigest;

/// Marker of a payload frame. A frame without it is refused before any decoding; there is no
/// version behind the marker, because the protocol is not versioned before 1.0: every wire
/// change is a total cutover, and the marker only tells a frame from foreign bytes and from the
/// link-control marker.
const PAYLOAD_FRAME_MARKER: &[u8] = b"RINGS-PAYLOAD\0";
/// Marker of a link-control frame. Distinct from [`PAYLOAD_FRAME_MARKER`] in its first
/// differing byte, so neither marker is a prefix of the other.
const LINK_CONTROL_FRAME_MARKER: &[u8] = b"RINGS-LINK\0";

/// The two session slots of a payload, as a product: `PerSlot<T> ≅ T × T`.
///
/// Every per-slot quantity (a reference, a lookup, a staged announcement) is carried in this
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

/// What a session slot holds on the wire.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum SessionRef<'a> {
    /// The session itself: the receiver learns it from this frame.
    Inline(Cow<'a, Session>),
    /// The content address of a session this link already carried inline.
    Digest(SessionDigest),
}

/// How a slot travelled: the shape of a [`SessionRef`] without its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotEncoding {
    /// The session was carried inline.
    Inline,
    /// The session was carried as its digest.
    Referenced,
}

impl SessionRef<'_> {
    /// How this slot travelled.
    pub(crate) const fn encoding(&self) -> SlotEncoding {
        match self {
            Self::Inline(_) => SlotEncoding::Inline,
            Self::Digest(_) => SlotEncoding::Referenced,
        }
    }
}

/// The wire form of a [`MessageVerification`]: the proof with its session slot as a reference.
#[derive(Deserialize, Serialize)]
struct WireVerification<'a> {
    /// The session slot.
    session: SessionRef<'a>,
    /// The proof's lifetime stamp, in the clear whether or not the slot is resolved.
    ttl_ms: u64,
    /// The proof's creation stamp.
    ts_ms: u128,
    /// The session signature.
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
    /// See [`Transaction::data`].
    data: Cow<'a, [u8]>,
    /// The origin's proof.
    verification: WireVerification<'a>,
}

/// The wire form of a [`MessagePayload`]: the same fields in the same order, with each session
/// slot a [`SessionRef`]. Borrowed when built by [`Self::view`], owned when decoded.
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
    fn view(verification: &'a MessageVerification, session: SessionRef<'a>) -> Self {
        Self {
            session,
            ttl_ms: verification.ttl_ms,
            ts_ms: verification.ts_ms,
            sig: Cow::Borrowed(verification.sig.as_slice()),
        }
    }

    /// The self-contained proof, with its slot resolved by `resolve`.
    fn resolve<E>(
        self,
        resolve: &mut impl FnMut(SessionRef<'a>) -> std::result::Result<Session, E>,
    ) -> std::result::Result<MessageVerification, E> {
        let Self {
            session,
            ttl_ms,
            ts_ms,
            sig,
        } = self;
        Ok(MessageVerification {
            session: resolve(session)?,
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
    pub(crate) fn view(payload: &'a MessagePayload, sessions: PerSlot<SessionRef<'a>>) -> Self {
        let transaction = &payload.transaction;
        Self {
            transaction: WireTransaction {
                destination: transaction.destination,
                tx_id: transaction.tx_id,
                sequence: transaction.sequence,
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
        Self::view(
            payload,
            payload
                .sessions()
                .map(|session| SessionRef::Inline(Cow::Borrowed(session))),
        )
    }

    /// The frame bytes: the payload marker, then the encoded body.
    pub(crate) fn to_wire(&self) -> Result<Bytes> {
        let body = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        frame_bytes(PAYLOAD_FRAME_MARKER, body.as_slice())
    }

    /// The exact length of [`Self::to_wire`], without allocating it.
    pub(crate) fn wire_size(&self) -> Result<usize> {
        let bytes = rings_codec::serialized_size(self).map_err(Error::CodecSerialize)?;
        let body = usize::try_from(bytes).map_err(|_| Error::MessageSizeOverflow)?;
        PAYLOAD_FRAME_MARKER
            .len()
            .checked_add(body)
            .ok_or(Error::MessageSizeOverflow)
    }

    /// What each slot holds.
    pub(crate) const fn session_refs(&self) -> PerSlot<&SessionRef<'a>> {
        PerSlot {
            origin: &self.transaction.verification.session,
            hop: &self.verification.session,
        }
    }

    /// The hop proof's lifetime, readable before the slots are resolved. It is unauthenticated
    /// until the payload verifies, so it may only bound how long an unresolved frame is kept.
    pub(crate) const fn hop_proof_lifetime(&self) -> ProofLifetime {
        ProofLifetime::new(self.verification.ts_ms, self.verification.ttl_ms)
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
        mut resolve: impl FnMut(SessionRef<'a>) -> std::result::Result<Session, E>,
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
                data: transaction.data.into_owned(),
                verification: origin,
            },
            relay: relay.into_owned(),
            verification: hop,
        })
    }
}

/// What the two ends of one link tell each other about session references. See the module
/// documentation for why these are unsigned.
#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) enum LinkControl {
    /// "A frame of yours carried this session inline and it verified: you may reference it."
    Known(SessionDigest),
    /// "A frame of yours references this digest and I cannot resolve it."
    Request(SessionDigest),
    /// "This is a session you asked for." The receiver recomputes the digest; it is never
    /// taken from the sender.
    Announce(Session),
    /// "I no longer hold the session you asked for": frames waiting on it cannot be resolved.
    Unknown(SessionDigest),
}

impl LinkControl {
    /// The frame bytes: the control marker, then the encoded body.
    pub(crate) fn to_wire(&self) -> Result<Bytes> {
        let body = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        frame_bytes(LINK_CONTROL_FRAME_MARKER, body.as_slice())
    }
}

/// One decoded frame.
pub(crate) enum LinkFrame {
    /// A payload whose session slots may still be references.
    Payload(Box<WirePayload<'static>>),
    /// A link-control frame.
    Control(LinkControl),
}

impl LinkFrame {
    /// Decode one frame by its marker.
    ///
    /// ```text
    ///   bytes ─┬─ PAYLOAD marker ──▶ Payload(decode body)
    ///          ├─ LINK marker ─────▶ Control(decode body)
    ///          └─ otherwise ───────▶ LegacyTransactionWireFormat
    /// ```
    pub(crate) fn from_wire(bytes: &[u8]) -> Result<Self> {
        if let Some(body) = bytes.strip_prefix(PAYLOAD_FRAME_MARKER) {
            return rings_codec::deserialize(body)
                .map(Box::new)
                .map(Self::Payload)
                .map_err(Error::CodecDeserialize);
        }
        if let Some(body) = bytes.strip_prefix(LINK_CONTROL_FRAME_MARKER) {
            return rings_codec::deserialize(body)
                .map(Self::Control)
                .map_err(Error::CodecDeserialize);
        }
        Err(Error::LegacyTransactionWireFormat)
    }
}

/// `marker || body`, with the length sum checked.
fn frame_bytes(marker: &[u8], body: &[u8]) -> Result<Bytes> {
    let capacity = marker
        .len()
        .checked_add(body.len())
        .ok_or(Error::MessageSizeOverflow)?;
    let mut wire = Vec::with_capacity(capacity);
    wire.extend_from_slice(marker);
    wire.extend_from_slice(body);
    Ok(Bytes::from(wire))
}

/// Resolve a slot of a frame that travels outside any link: only an inline session resolves.
pub(super) fn resolve_inline(session: SessionRef<'_>) -> Result<Session> {
    match session {
        SessionRef::Inline(session) => Ok(session.into_owned()),
        SessionRef::Digest(digest) => Err(Error::SessionReferenceUnresolved(digest)),
    }
}
