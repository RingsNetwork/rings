//! The relay inbox: messages held for a peer while it is offline.
//!
//! Model: an application message addressed to `d` routes to `succ(d)`. When `d` has no
//! connection at all, `succ(d)` is responsible for the position `d` occupies but cannot deliver,
//! so it *holds* the message: it wraps the payload in a [`HeldMessage`] carrying its own
//! signature and the hold instant, and writes it into the carrier `inbox(d) = d + 1`
//! (arithmetic in `Z / 2^160`, kind [`EntryKind::RelayMessage`]) through the ordinary storage
//! write path. Relay carriers are placed by the ring geometry alone, `k ∈ (n, succ(n)]`, in every
//! storage mode, so while `d` is offline the carrier lives at `d`'s predecessor and once `d`
//! returns `d + 1 ∈ (d, succ(d)]` makes `d` itself the owner: the storage repair pass of the
//! predecessor hands the carrier to `d`, which delivers it through its inbound pipeline and
//! tombstones the delivered elements.
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
//! from the node the owner itself routes `d` to (`find_successor(d)` answered locally), a
//! relocation of the carrier only from the owner's predecessor as an ownership hand-off, and a
//! removal only from `d`; a relay carrier is never fetched, cached, or replicated. Removal is
//! per element by its add dot (an observed-remove), never by a reset floor, so a message the
//! recipient has not seen is never dropped by a compaction it did not issue.

use serde::Deserialize;
use serde::Serialize;

use super::Entry;
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
    /// Hold `payload` under `holder`'s authority, stamped with the current instant.
    pub(crate) fn hold(payload: MessagePayload, holder: MessageSigner<&SessionSk>) -> Result<Self> {
        let data = rings_codec::serialize(&payload).map_err(Error::CodecSerialize)?;
        Ok(Self {
            holder: holder.sign(HELD_MESSAGE_DOMAIN_TAG, &data)?,
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
    /// Post: `Ok(())` implies the payload is a `CustomMessage` addressed to `destination`, the
    /// hold instant is at most `now_ms + TS_OFFSET_TOLERANCE_MS`, the holder signature verifies
    /// inside `network_id` as of the hold instant, and the payload's transaction and hop
    /// signatures verify inside `network_id` as of the hold instant.
    pub(crate) fn validate_witness(
        &self,
        destination: Did,
        now_ms: u128,
        network_id: u32,
    ) -> Result<()> {
        if self.payload.transaction.destination != destination {
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

    /// The element witness over every element of a relay carrier, and the carrier shape a relay
    /// carrier must keep: no reset floor, because removal is by dot only.
    ///
    /// Pre: `self.kind == EntryKind::RelayMessage`.
    pub(super) fn validate_inbox_witness(&self, now_ms: u128, network_id: u32) -> Result<()> {
        if self.crdt.register.is_some() {
            return Err(Error::RelayInboxRegisterNotAllowed);
        }
        let destination = inbox_destination(self.did);
        self.data.iter().try_for_each(|element| {
            verified_element(element, destination, now_ms, network_id).map(drop)
        })
    }

    /// Whether every element of this relay delta was held by `holder`.
    ///
    /// Pre: `self` passed [`Self::validate_inbox_witness`], so every element decodes.
    pub(crate) fn every_element_held_by(&self, holder: Did) -> bool {
        self.data.iter().all(|element| {
            HeldMessage::from_encoded(element).is_ok_and(|held| held.holder() == holder)
        })
    }

    /// Partition this inbox into the messages its recipient may deliver and the dots of every
    /// element it must retire: the delivered ones and the ones that fail the witness under the
    /// recipient's overlay (junk a misbehaving owner relocated).
    ///
    /// Post: `deliverable` is in carrier order; `retired` names every element of the carrier.
    pub(crate) fn drain_inbox(&self, now_ms: u128, network_id: u32) -> InboxDrain {
        let destination = inbox_destination(self.did);
        let mut deliverable = Vec::new();
        let mut rejected = 0usize;
        for element in &self.data {
            match verified_element(element, destination, now_ms, network_id) {
                Ok(held) => deliverable.push(held.payload),
                Err(_) => rejected = rejected.saturating_add(1),
            }
        }
        InboxDrain {
            deliverable,
            rejected,
            retired: self.tombstone_delta(),
        }
    }

    /// The removal delta naming every element of this carrier by its add dot.
    fn tombstone_delta(&self) -> Entry {
        let mut removal = Self::new(self.did, Vec::new(), EntryKind::RelayMessage);
        removal.crdt.dots = self.crdt.dots.clone();
        removal
    }
}

/// The outcome of draining an inbox: what to deliver and what to retire.
#[derive(Debug)]
pub(crate) struct InboxDrain {
    /// Messages that pass the witness, in carrier order.
    pub(crate) deliverable: Vec<MessagePayload>,
    /// Elements that failed the witness under the recipient's overlay.
    pub(crate) rejected: usize,
    /// The tombstone delta retiring every element of the drained carrier.
    pub(crate) retired: Entry,
}

impl EntryOperation {
    /// The authority law of a relay-carrier operation: a hold (`Extend`) only from the node the
    /// owner routes the destination to (`responsible`, `None` when that route is not local), a
    /// removal (`Tombstone`) only from the recipient, and no other operation.
    ///
    /// Pre: the carried entry passed the element witness.
    pub(crate) fn validate_inbox_authority(
        &self,
        writer: Did,
        responsible: Option<Did>,
    ) -> Result<()> {
        let entry = self.entry();
        match self {
            EntryOperation::Extend(_) => {
                let holds = responsible
                    .is_some_and(|node| writer == node && entry.every_element_held_by(node));
                holds
                    .then_some(())
                    .ok_or(Error::RelayMessageHolderNotResponsible)
            }
            EntryOperation::Tombstone(_) => (writer == inbox_destination(entry.did))
                .then_some(())
                .ok_or(Error::RelayInboxWriterNotRecipient),
            EntryOperation::Overwrite(_)
            | EntryOperation::Touch(_)
            | EntryOperation::CompactData(_) => Err(Error::RelayInboxOperationNotAllowed),
        }
    }
}

/// The authority law of a relay-carrier relocation: an ownership hand-off is accepted only from
/// the receiver's predecessor, the owner whose interval the carrier is leaving.
pub(crate) fn validate_inbox_relocation(origin: Did, predecessor: Option<Did>) -> Result<()> {
    (predecessor == Some(origin))
        .then_some(())
        .ok_or(Error::RelayInboxNotRelocatable)
}
