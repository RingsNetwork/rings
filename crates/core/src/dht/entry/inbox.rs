//! The relay inbox: messages held for a peer while it is offline.
//!
//! Model: an application message addressed to `d` routes to `succ(d)`. When `d` has no
//! connection at all, `succ(d)` is responsible for the position `d` occupies but cannot deliver,
//! so it *holds* the message: it wraps the payload in a [`HeldMessage`] carrying its own
//! signature and the hold instant, and writes it into the carrier `inbox(d) = d + 1`
//! (arithmetic in `Z / 2^160`, kind [`EntryKind::RelayMessage`]) through the ordinary storage
//! write path. Relay carriers are placed by the ring geometry alone, `k ∈ (n, succ(n)]`, in every
//! storage mode, at that one placement whatever the configured redundancy, and in their own
//! storage namespace, so a data topic parked at `d + 1` (any node may name any position through
//! an overwrite) never shadows the inbox kept for `d`. While `d` is offline the carrier lives at
//! `d`'s predecessor and once `d` returns `d + 1 ∈ (d, succ(d)]` makes `d` itself the owner: the
//! storage repair pass of the predecessor hands the carrier to `d`, which delivers it through its
//! inbound pipeline and tombstones the delivered elements.
//!
//! Only [`Message::CustomMessage`] is held; every other message class (control, storage, E2E
//! frames) is forwarded as before and lost at a dead end, which is the pre-existing behaviour.
//!
//! Witness: the kind of an entry is a peer-declared wire field, so the inbox policy (longer
//! retention than a data topic, a writer restricted to one node) is safe only because every
//! element is verifiable by whoever stores it. An element is admissible iff, as an
//! [`Entry`] element, it decodes as a [`HeldMessage`] whose payload is a `CustomMessage`
//! addressed to `inbox_destination(entry.did)`, whose hold instant is not ahead of the
//! receiver's clock, whose holder signature verifies inside the receiver's overlay, and whose
//! payload verifies *as of the hold instant*: both inner proofs were live then and both inner
//! sessions were authorized then. Judging the payload at the hold instant keeps admissibility
//! monotone in time (a sender's session expiring later does not unverify a held message) and
//! bounds replay: a holder can only hold a message inside its sender's own proof lifetime.
//!
//! Authority (checked by the shell, which knows who is writing): a fresh hold is admitted only
//! from the node the owner itself routes `d` to (`find_successor(d)` answered locally), only
//! while the held message's sender proof is still live by the owner's clock (so the hold instant
//! a holder signs cannot lie about the past), a relocation of the carrier only from the owner's
//! authenticated predecessor as an ownership hand-off, and a removal only from `d`; a relay
//! carrier is never fetched, cached, or replicated. Removal is per element by its add dot (an
//! observed-remove), never by a reset floor, so a message the recipient has not seen is never
//! dropped by a compaction it did not issue.
//!
//! Both "responsible for `d`" (the holder's `(pred, self]`) and "routes `d` to" (the owner's
//! successor list) are projections of failure detection: while an owner still lists the departed
//! `d` as its head, it routes `d` to `d` and refuses the hold, and the message is lost as it was
//! before the inbox existed. The window closes when the owner retires `d`.

use serde::Deserialize;
use serde::Serialize;

use super::Entry;
use super::EntryDot;
use super::EntryKind;
use super::EntryOperation;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Decoder;
use crate::message::DomainTag;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::MessageVerification;
use crate::message::MessageVerificationExt;
use crate::session::SessionSk;

/// The message family of a holder's signature over a held payload.
pub(crate) const HELD_MESSAGE_DOMAIN_TAG: DomainTag =
    crate::domain_tag!("rings-core:relay-inbox:held-message:v1");

/// The relay-inbox carrier of `destination`: the ring position just after it.
pub(crate) fn inbox_key(destination: Did) -> Did {
    destination + Did::from(1u32)
}

/// The peer an inbox carrier is kept for; the inverse of [`inbox_key`].
pub(crate) fn inbox_destination(inbox: Did) -> Did {
    inbox - Did::from(1u32)
}

/// One element of a relay inbox: an application payload together with its holder's attestation.
///
/// Invariant (established by [`HeldMessage::hold`], re-derived by [`Entry`] admission): `holder`
/// signs the canonical encoding of `payload` under [`HELD_MESSAGE_DOMAIN_TAG`]; its timestamp is
/// the instant the holder received the payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldMessage {
    /// The application payload as it reached the holder.
    pub(crate) payload: MessagePayload,
    /// The holder's signature; `ts_ms` is the hold instant.
    pub(crate) holder: MessageVerification,
}

impl MessageVerificationExt for HeldMessage {
    const DOMAIN_TAG: DomainTag = HELD_MESSAGE_DOMAIN_TAG;

    fn verification_data(&self) -> Result<Vec<u8>> {
        rings_codec::serialize(&self.payload).map_err(Error::CodecSerialize)
    }

    fn verification(&self) -> &MessageVerification {
        &self.holder
    }
}

impl Encoder for HeldMessage {
    fn encode(&self) -> Result<Encoded> {
        rings_codec::serialize(self)
            .map_err(Error::CodecSerialize)?
            .encode()
    }
}

impl Decoder for HeldMessage {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let wire: Vec<u8> = encoded.decode()?;
        rings_codec::deserialize(&wire).map_err(Error::CodecDeserialize)
    }
}

impl HeldMessage {
    /// Hold `payload` under `holder`'s authority at the instant `held_at_ms`.
    pub(crate) fn hold(
        payload: MessagePayload,
        holder: MessageSigner<&SessionSk>,
        held_at_ms: u128,
    ) -> Result<Self> {
        let data = rings_codec::serialize(&payload).map_err(Error::CodecSerialize)?;
        Ok(Self {
            holder: holder.sign_at(HELD_MESSAGE_DOMAIN_TAG, &data, held_at_ms)?,
            payload,
        })
    }

    /// The node that held the payload.
    pub(crate) fn holder(&self) -> Did {
        self.signer()
    }

    /// The instant the holder received the payload.
    pub(crate) const fn held_at_ms(&self) -> u128 {
        self.holder.ts_ms
    }

    /// The element witness (see the module documentation).
    ///
    /// Pre: `now_ms` is the receiver's clock and `network_id` its overlay.
    /// Post: `Ok(())` implies the payload is a `CustomMessage` whose transaction and relay are
    /// both addressed to `destination` (so its recipient has nothing to forward), the
    /// hold instant is at most `now_ms + TS_OFFSET_TOLERANCE_MS`, the holder signature verifies
    /// inside `network_id` as of the hold instant, and the payload's transaction and hop
    /// signatures verify inside `network_id` as of the hold instant.
    pub(crate) fn validate_witness(
        &self,
        destination: Did,
        now_ms: u128,
        network_id: u32,
    ) -> Result<()> {
        if self.payload.transaction.destination != destination
            || self.payload.relay.destination != destination
        {
            return Err(Error::RelayMessageNotAddressedToInbox);
        }
        if !matches!(
            self.payload.transaction.data::<Message>()?,
            Message::CustomMessage(_)
        ) {
            return Err(Error::RelayMessageNotCustom);
        }
        let held_at_ms = self.held_at_ms();
        if held_at_ms > now_ms.saturating_add(TS_OFFSET_TOLERANCE_MS) {
            return Err(Error::RelayMessageHeldAheadOfClock);
        }
        if !self.verify_at(network_id, held_at_ms) {
            return Err(Error::RelayMessageUnverifiable);
        }
        if !(self.payload.transaction.verify_at(network_id, held_at_ms)
            && self.payload.verify_at(network_id, held_at_ms))
        {
            return Err(Error::RelayMessageHeldOutsideSenderProof);
        }
        Ok(())
    }
}

/// Decode one inbox element and check its witness.
fn verified_element(
    element: &Encoded,
    destination: Did,
    now_ms: u128,
    network_id: u32,
) -> Result<HeldMessage> {
    let held = HeldMessage::from_encoded(element)?;
    held.validate_witness(destination, now_ms, network_id)?;
    Ok(held)
}

impl Entry {
    /// The inbox delta holding one message for its destination.
    ///
    /// Post: `result.did = inbox_key(destination)` and the single element is `held` in wire
    /// encoding, so the storage owner re-derives the witness from the element alone.
    pub(crate) fn inbox_delta(held: &HeldMessage) -> Result<Self> {
        Ok(Self::new(
            inbox_key(held.payload.transaction.destination),
            vec![held.encode()?],
            EntryKind::RelayMessage,
        ))
    }

    /// Every element of this relay carrier, decoded and witnessed, in carrier order; and the
    /// carrier shape a relay carrier must keep: no reset floor, because removal is by dot only,
    /// and no more elements than the inbox keeps.
    ///
    /// Pre: `self.kind == EntryKind::RelayMessage`.
    pub(crate) fn witnessed_inbox_elements(
        &self,
        now_ms: u128,
        network_id: u32,
    ) -> Result<Vec<HeldMessage>> {
        if self.crdt.register.is_some() {
            return Err(Error::RelayInboxRegisterNotAllowed);
        }
        // Every element costs signature verifications; a delta the inbox could not keep is
        // refused before the first one. (A data topic caps at materialization instead: its
        // elements cost nothing to admit.)
        if self.data.len() > self.kind.max_data_len() {
            return Err(Error::RelayInboxDeltaExceedsCapacity);
        }
        let destination = inbox_destination(self.did);
        self.data
            .iter()
            .map(|element| verified_element(element, destination, now_ms, network_id))
            .collect()
    }

    /// The element witness over every element of a relay carrier.
    pub(super) fn validate_inbox_witness(&self, now_ms: u128, network_id: u32) -> Result<()> {
        self.witnessed_inbox_elements(now_ms, network_id).map(drop)
    }

    /// Partition this stored inbox into the elements its recipient may deliver, each with the
    /// add dot that retires it, and the removal delta of every element that fails the witness
    /// under the recipient's overlay (junk a misbehaving owner relocated).
    ///
    /// Pre: `self` is the materialized stored carrier, so `data` and `crdt.dots` align.
    /// Post: `deliverable` is in carrier order; `deliverable` dots and `rejected` dots together
    /// are every dot of the carrier.
    pub(crate) fn partition_inbox(&self, now_ms: u128, network_id: u32) -> InboxDrain {
        let destination = inbox_destination(self.did);
        let mut deliverable = Vec::new();
        let mut rejected = Vec::new();
        for (element, dot) in self.data.iter().zip(self.crdt.dots.iter().copied()) {
            match verified_element(element, destination, now_ms, network_id) {
                Ok(held) => deliverable.push(InboxElement {
                    dot,
                    payload: held.payload,
                }),
                Err(_) => rejected.push(dot),
            }
        }
        InboxDrain {
            deliverable,
            rejected: self.removal_of(rejected),
        }
    }

    /// The removal delta of this carrier naming `dots`.
    pub(crate) fn removal_of(&self, dots: impl IntoIterator<Item = EntryDot>) -> Entry {
        let mut removal = Self::new(self.did, Vec::new(), EntryKind::RelayMessage);
        removal.crdt.dots = dots.into_iter().collect();
        removal
    }
}

/// One deliverable inbox element: the payload and the add dot that retires it once delivered.
#[derive(Debug)]
pub(crate) struct InboxElement {
    /// The add dot of the element in the stored carrier.
    pub(crate) dot: EntryDot,
    /// The held application payload.
    pub(crate) payload: MessagePayload,
}

/// The outcome of partitioning an inbox: what to deliver and what to retire unread.
#[derive(Debug)]
pub(crate) struct InboxDrain {
    /// Elements that pass the witness, in carrier order.
    pub(crate) deliverable: Vec<InboxElement>,
    /// The removal delta of the elements that failed the witness; empty when none did.
    pub(crate) rejected: Entry,
}

impl EntryOperation {
    /// The write law of a relay-carrier operation issued by `writer` at the owner's clock
    /// `now_ms`: a hold (`Extend`) only from the node the owner routes the destination to
    /// (`responsible`, `None` when that route is not local), every element held by that node,
    /// and held while its sender's proof is still live by the owner's own clock; a removal
    /// (`Tombstone`) only from the recipient; no other operation.
    ///
    /// The freshness bound is what makes the hold instant honest: the holder signs it, so the
    /// witness alone would let a holder judge an old message at a time of its choosing. Judged
    /// once here at the write, it bounds replay by the sender's proof lifetime; relocation and
    /// delivery then judge as of the recorded instant, which keeps them monotone in time.
    ///
    /// Post: `Ok(())` implies the carried entry passed the element witness; authority is judged
    /// before any signature is verified, so a stranger costs the owner no verification.
    pub(crate) fn validate_inbox_write(
        &self,
        writer: Did,
        responsible: Option<Did>,
        now_ms: u128,
        network_id: u32,
    ) -> Result<()> {
        match self {
            EntryOperation::Extend(entry) => {
                if responsible != Some(writer) {
                    return Err(Error::RelayMessageHolderNotResponsible);
                }
                for held in entry.witnessed_inbox_elements(now_ms, network_id)? {
                    if held.holder() != writer {
                        return Err(Error::RelayMessageHolderNotResponsible);
                    }
                    if !held.payload.transaction.verification.is_live_at(now_ms) {
                        return Err(Error::RelayMessageHoldStale);
                    }
                }
                Ok(())
            }
            EntryOperation::Tombstone(entry) => {
                if writer != inbox_destination(entry.did) {
                    return Err(Error::RelayInboxWriterNotRecipient);
                }
                entry.validate_inbox_witness(now_ms, network_id)
            }
            EntryOperation::Overwrite(_)
            | EntryOperation::Touch(_)
            | EntryOperation::CompactData(_) => Err(Error::RelayInboxOperationNotAllowed),
        }
    }
}

/// The authority law of a relay-carrier relocation: an ownership hand-off is accepted only from
/// the receiver's predecessor, the owner whose interval the carrier is leaving. A hand-off from
/// anyone else is not invalid, only not this receiver's to take yet, so the law is a predicate
/// the batch skips on rather than an error it fails with.
///
/// Pre: `sender` is the authenticated signer of the hand-off, not its peer-declared relay path.
pub(crate) fn relocates_from_predecessor(sender: Did, predecessor: Option<Did>) -> bool {
    predecessor == Some(sender)
}
