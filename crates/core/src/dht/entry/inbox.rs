//! The relay inbox: messages held for a peer while it is offline.
//!
//! Model: a message addressed to `d` routes to `succ(d)`. When `d` is not connected, `succ(d)`
//! is responsible for the position `d` occupies but cannot deliver, so it holds the message in
//! the carrier `inbox(d) = d + 1` (arithmetic in `Z / 2^160`) through the ordinary storage write
//! path. Storage places a key `k` at the node `n` with `k ∈ (n, succ(n)]`, so while `d` is offline
//! the carrier lives at `d`'s predecessor, and once `d` returns `d + 1 ∈ (d, succ(d)]` makes `d`
//! itself the owner: ownership hand-off moves the carrier to `d`, which drains it from its own
//! storage, delivers the messages to its application, and compacts the carrier.
//!
//! Witness: the kind of an entry is a peer-declared wire field, so the inbox policy (longer
//! retention than a data topic) is safe only because every element is verifiable by the
//! storage owner without trusting the writer. An element is admitted iff it decodes as a
//! [`MessagePayload`] whose transaction is addressed to `inbox_destination(entry.did)`, carries an
//! application message, and whose transaction and hop signatures verify inside the owner's
//! overlay. Signature liveness is deliberately not required: the inbox exists precisely to hold
//! a message past its own proof lifetime, and the recipient re-verifies the same signatures when
//! it drains the inbox.

use super::Entry;
use super::EntryKind;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::message::Decoder;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessageClass;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;

/// The relay-inbox carrier of `destination`: the ring position just after it.
pub fn inbox_key(destination: Did) -> Did {
    destination + Did::from(1u32)
}

/// The peer an inbox carrier is kept for; the inverse of [`inbox_key`].
pub fn inbox_destination(inbox: Did) -> Did {
    inbox - Did::from(1u32)
}

/// Decode one inbox element and check the witness under `network_id`.
///
/// Post: `Ok(payload)` implies the payload is addressed to `destination`, carries an application
/// message, and both its signatures verify inside `network_id` regardless of proof liveness.
pub fn verified_inbox_message(
    element: &Encoded,
    destination: Did,
    network_id: u32,
) -> Result<MessagePayload> {
    let payload = MessagePayload::from_encoded(element)?;
    if payload.transaction.destination != destination {
        return Err(Error::RelayMessageNotAddressedToInbox);
    }
    if payload.transaction.data::<Message>()?.kind().class() != MessageClass::Application {
        return Err(Error::RelayMessageNotApplication);
    }
    if !(payload.transaction.verify_signature(network_id) && payload.verify_signature(network_id)) {
        return Err(Error::RelayMessageUnverifiable);
    }
    Ok(payload)
}

impl Entry {
    /// The inbox delta holding one undeliverable `payload` for its destination.
    ///
    /// Post: `result.did = inbox_key(payload.transaction.destination)` and the single element is
    /// `payload` in wire encoding, so the storage owner can re-derive the witness.
    pub fn inbox_delta(payload: &MessagePayload) -> Result<Self> {
        Ok(Self::new(
            inbox_key(payload.transaction.destination),
            vec![payload.encode()?],
            EntryKind::RelayMessage,
        ))
    }

    /// The witness every element of a relay inbox must satisfy (see the module documentation).
    ///
    /// Pre: `self.kind == EntryKind::RelayMessage`.
    pub(super) fn validate_inbox_witness(&self, network_id: u32) -> Result<()> {
        let destination = inbox_destination(self.did);
        self.data.iter().try_for_each(|element| {
            verified_inbox_message(element, destination, network_id).map(drop)
        })
    }

    /// The messages a recipient may deliver from its inbox: every element that satisfies the
    /// witness under the recipient's overlay, in carrier order.
    ///
    /// Post: elements that fail the witness are skipped, never delivered; the carrier's own
    /// admission makes them unreachable through an honest owner, so a failure here names a
    /// misbehaving owner rather than a stale message.
    pub fn deliverable_inbox_messages(&self, network_id: u32) -> Vec<MessagePayload> {
        let destination = inbox_destination(self.did);
        self.data
            .iter()
            .filter_map(|element| verified_inbox_message(element, destination, network_id).ok())
            .collect()
    }
}
