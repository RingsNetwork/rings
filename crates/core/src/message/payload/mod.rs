#![deny(missing_docs)]

use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;
#[cfg(test)]
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use super::encoder::Decoder;
use super::encoder::Encoded;
use super::encoder::Encoder;
use super::protocols::DomainTag;
use super::protocols::HopBudget;
use super::protocols::MessageRelay;
use super::protocols::MessageSigner;
use super::protocols::MessageVerification;
use super::protocols::MessageVerificationExt;
use super::replay::StreamKey;
use super::replay::TransactionDigest;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::domain_tag;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

mod wire;

pub(crate) use self::wire::DelegationRef;
pub(crate) use self::wire::LinkControl;
pub(crate) use self::wire::LinkFrame;
pub(crate) use self::wire::PerSlot;
pub(crate) use self::wire::SlotEncoding;
pub(crate) use self::wire::WirePayload;

/// Message family of the [`Transaction`] signature: the origin's authorship of a message.
const TRANSACTION_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-core:message-verification:transaction");
/// Message family of the [`MessagePayload`] signature: one hop's authorship of a forwarded
/// envelope. Distinct from [`TRANSACTION_DOMAIN_TAG`] so the two signatures over the same
/// transaction hash are never interchangeable.
const PAYLOAD_DOMAIN_TAG: DomainTag = domain_tag!("rings-core:message-verification:payload");
#[cfg(test)]
static TEST_TRANSACTION_SEQUENCES: LazyLock<Mutex<std::collections::BTreeMap<StreamKey, u64>>> =
    LazyLock::new(|| Mutex::new(std::collections::BTreeMap::new()));

#[cfg(test)]
fn next_test_transaction_sequence(key: StreamKey) -> Result<u64> {
    let mut sequences = TEST_TRANSACTION_SEQUENCES
        .lock()
        .map_err(|_| Error::TransactionReplayStateInvalid)?;
    let Some(last) = sequences.get_mut(&key) else {
        sequences.insert(key, 0);
        return Ok(0);
    };
    let next = last
        .checked_add(1)
        .ok_or(Error::TransactionSequenceExhausted { key })?;
    *last = next;
    Ok(next)
}

fn hash_transaction(destination: Did, tx_id: uuid::Uuid, sequence: u64, data: &[u8]) -> [u8; 32] {
    let mut msg = vec![];

    msg.extend_from_slice(destination.as_bytes());
    msg.extend_from_slice(tx_id.as_bytes());
    msg.extend_from_slice(&sequence.to_be_bytes());
    msg.extend_from_slice(data);

    keccak256(&msg)
}

/// All messages transmitted in RingsNetwork should be wrapped by `Transaction`.
/// It additionally offer destination, tx_id and verification.
///
/// A report for a transaction is routed to the transaction's [origin](Self::origin); no other
/// return address exists, so a request can never direct a report at a third party.
///
/// To transmit `Transaction` in RingsNetwork, user should build
/// [MessagePayload] and use [PayloadSender] to send.
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// The destination of this message.
    pub destination: Did,
    /// The transaction ID.
    /// Remote peer should use same tx_id when response.
    pub tx_id: uuid::Uuid,
    /// Monotonic sequence inside the origin account's destination-scoped stream.
    pub sequence: u64,
    /// data
    pub data: Vec<u8>,
    /// This field holds a signature from a node,
    /// which is used to prove that the transaction was created by that node.
    pub verification: MessageVerification,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("destination", &self.destination)
            .field("tx_id", &self.tx_id)
            .field("sequence", &self.sequence)
            .field("data_bytes", &self.data.len())
            .finish()
    }
}

/// `MessagePayload` is used to transmit data between nodes.
/// The data should be packed by [Transaction].
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct MessagePayload {
    /// Payload data
    pub transaction: Transaction,
    /// The relay carrier: the next hop, the destination, and the forwards left.
    pub relay: MessageRelay,
    /// This field holds a signature from a node,
    /// which is used to prove that payload was created by that node.
    pub verification: MessageVerification,
}

impl fmt::Debug for MessagePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagePayload")
            .field("transaction", &self.transaction)
            .field("relay", &self.relay)
            .finish()
    }
}

impl Transaction {
    /// Wrap data. Will serialize by [rings_codec::serialize]
    /// then sign [MessageVerification] by `signer`.
    pub fn new<T>(
        destination: Did,
        tx_id: uuid::Uuid,
        sequence: u64,
        data: T,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let data = rings_codec::serialize(&data).map_err(Error::CodecSerialize)?;
        let msg_hash = hash_transaction(destination, tx_id, sequence, &data);
        let verification = signer.sign(TRANSACTION_DOMAIN_TAG, &msg_hash)?;
        Ok(Self {
            destination,
            tx_id,
            sequence,
            data,
            verification,
        })
    }

    /// The origin of this transaction: the account that authorized the session it is signed
    /// by, i.e. [`MessageVerificationExt::signer`] under the name the routing layer uses for it.
    /// This is the ring position a request is authorized against and the address its report is
    /// routed to; it is never the session id, which names a key, not a node.
    pub fn origin(&self) -> Did {
        self.signer()
    }

    /// Destination-scoped stream identity under the receiver's overlay.
    pub fn stream_key(&self, network_id: u32) -> StreamKey {
        StreamKey::new(network_id, self.origin(), self.destination)
    }

    /// Digest of this exact signed transaction, including its delegation and signature.
    pub fn digest(&self) -> Result<TransactionDigest> {
        let wire = rings_codec::serialize(self).map_err(Error::CodecSerialize)?;
        Ok(TransactionDigest::new(keccak256(&wire)))
    }

    /// Deserializes the data field into a `T` instance.
    pub fn data<T>(&self) -> Result<T>
    where T: DeserializeOwned {
        rings_codec::deserialize(&self.data).map_err(Error::CodecDeserialize)
    }
}

impl MessagePayload {
    /// Create new `MessagePayload`.
    /// Need [Transaction], [MessageSigner] and [MessageRelay].
    pub fn new(
        transaction: Transaction,
        signer: MessageSigner<&DelegateeKey>,
        relay: MessageRelay,
    ) -> Result<Self> {
        let msg_hash = hash_transaction(
            transaction.destination,
            transaction.tx_id,
            transaction.sequence,
            &transaction.data,
        );
        let verification = signer.sign(PAYLOAD_DOMAIN_TAG, &msg_hash)?;
        Ok(Self {
            transaction,
            relay,
            verification,
        })
    }

    /// Helps to create sending message from data: a fresh carrier with the full hop budget.
    pub fn new_send_with_sequence<T>(
        data: T,
        signer: MessageSigner<&DelegateeKey>,
        next_hop: Did,
        destination: Did,
        sequence: u64,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let tx_id = crate::utils::new_uuid();
        let transaction = Transaction::new(destination, tx_id, sequence, data, signer)?;
        let relay = MessageRelay::new(next_hop, transaction.destination, HopBudget::MAX);
        Self::new(transaction, signer, relay)
    }

    #[cfg(test)]
    pub(crate) fn new_send<T>(
        data: T,
        signer: MessageSigner<&DelegateeKey>,
        next_hop: Did,
        destination: Did,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let sequence = next_test_transaction_sequence(StreamKey::new(
            signer.network_id(),
            signer.delegator_did(),
            destination,
        ))?;
        Self::new_send_with_sequence(data, signer, next_hop, destination, sequence)
    }

    /// The sessions in the two slots of this payload: the origin's and the current hop's.
    pub(crate) const fn delegations(&self) -> PerSlot<&Delegation> {
        PerSlot {
            origin: &self.transaction.verification.delegation,
            hop: &self.verification.delegation,
        }
    }

    /// Deserializes a self-contained `MessagePayload` from the Rings wire encoding: a payload
    /// frame with both sessions inline.
    ///
    /// A frame that references a session is meaningful only on the link that announced it, and
    /// is refused here with [`Error::DelegationReferenceUnresolved`]; a link resolves such frames
    /// before they reach a `MessagePayload`.
    pub fn from_wire(data: &[u8]) -> Result<Self> {
        match LinkFrame::from_wire(data)? {
            LinkFrame::Payload(frame) => frame.into_self_contained(),
            LinkFrame::Control(_) => Err(Error::LinkControlOutsideLink),
        }
    }

    /// Serializes the `MessagePayload` into the self-contained Rings wire encoding: a payload
    /// frame with both sessions inline, valid on any link and outside one.
    pub fn to_wire(&self) -> Result<Bytes> {
        WirePayload::inline(self).to_wire()
    }

    /// The exact length of [`Self::to_wire`], without allocating the wire buffer. A link may
    /// send fewer bytes (a referenced session is shorter than an inline one), never more.
    pub(crate) fn wire_size(&self) -> Result<usize> {
        WirePayload::inline(self).wire_size()
    }

    /// Returns whether `local` is the relay destination of this payload.
    pub(crate) fn is_relay_destination_for(&self, local: Did) -> bool {
        self.relay.destination == local
    }

    /// Returns whether `local` should forward this payload to another node.
    pub(crate) fn should_forward_from(&self, local: Did) -> bool {
        !self.is_relay_destination_for(local)
    }

    /// Verify both the immutable origin transaction and the current payload carrier.
    pub(crate) fn verify_transaction_and_payload(&self, network_id: u32) -> bool {
        self.transaction.verify(network_id) && MessageVerificationExt::verify(self, network_id)
    }
}

impl MessageVerificationExt for Transaction {
    const DOMAIN_TAG: DomainTag = TRANSACTION_DOMAIN_TAG;

    fn verification_data(&self) -> Result<Vec<u8>> {
        Ok(hash_transaction(self.destination, self.tx_id, self.sequence, &self.data).to_vec())
    }

    fn verification(&self) -> &MessageVerification {
        &self.verification
    }
}

impl MessageVerificationExt for MessagePayload {
    const DOMAIN_TAG: DomainTag = PAYLOAD_DOMAIN_TAG;

    fn verification_data(&self) -> Result<Vec<u8>> {
        self.transaction.verification_data()
    }

    fn verification(&self) -> &MessageVerification {
        &self.verification
    }
}

impl Encoder for MessagePayload {
    fn encode(&self) -> Result<Encoded> {
        self.to_wire()?.encode()
    }
}

impl Decoder for MessagePayload {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Bytes = encoded.decode()?;
        Self::from_wire(&v)
    }
}

/// Trait of PayloadSender
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait PayloadSender {
    /// The authority that signs every payload this sender emits.
    fn message_signer(&self) -> MessageSigner<&DelegateeKey>;

    /// Get access to DHT.
    fn dht(&self) -> Arc<PeerRing>;

    /// Used to check if destination is already connected when `infer_next_hop`
    fn is_connected(&self, did: Did) -> bool;

    /// Persistently reserve sender sequences for one final destination before signing.
    async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        count: NonZeroU64,
    ) -> Result<std::ops::RangeInclusive<u64>>;

    /// Send a message payload to a specified DID.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()>;

    /// Infer the next hop for a message by calling `dht.find_successor()`.
    fn infer_next_hop(&self, destination: Did, next_hop: Option<Did>) -> Result<Did> {
        if self.is_connected(destination) {
            return Ok(destination);
        }

        if let Some(next_hop) = next_hop {
            return Ok(next_hop);
        }

        match self.dht().find_successor(destination)? {
            PeerRingAction::Some(did) => Ok(did),
            PeerRingAction::RemoteAction(did, _) => Ok(did),
            _ => Err(Error::NoNextHop),
        }
    }

    /// Alias for `do_send_payload` that sets the next hop to `payload.relay.next_hop`.
    async fn send_payload(&self, payload: MessagePayload) -> Result<()> {
        self.do_send_payload(payload.relay.next_hop, payload).await
    }

    /// Send a message to a specified destination by specified next hop.
    async fn send_message_by_hop<T>(
        &self,
        msg: T,
        destination: Did,
        next_hop: Did,
    ) -> Result<uuid::Uuid>
    where
        T: Serialize + Send,
    {
        let sequence = *self
            .reserve_transaction_sequences(destination, NonZeroU64::MIN)
            .await?
            .start();
        let payload = MessagePayload::new_send_with_sequence(
            msg,
            self.message_signer(),
            next_hop,
            destination,
            sequence,
        )?;
        let tx_id = payload.transaction.tx_id;
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// Send a message to a specified destination.
    async fn send_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        let next_hop = self.infer_next_hop(destination, None)?;
        self.send_message_by_hop(msg, destination, next_hop).await
    }

    /// Send a direct message to a specified destination.
    async fn send_direct_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        self.send_message_by_hop(msg, destination, destination)
            .await
    }

    /// Send a report for the request carried by `payload`: a fresh payload Chord-routed to the
    /// request's origin under the same transaction id.
    async fn send_report_message<T>(&self, payload: &MessagePayload, msg: T) -> Result<()>
    where T: Serialize + Send {
        let origin = payload.transaction.origin();
        let next_hop = self.infer_next_hop(origin, None)?;
        let relay = payload.relay.report(self.dht().did, origin, next_hop)?;

        let signer = self.message_signer();
        let sequence = *self
            .reserve_transaction_sequences(origin, NonZeroU64::MIN)
            .await?
            .start();
        let transaction =
            Transaction::new(origin, payload.transaction.tx_id, sequence, msg, signer)?;

        let pl = MessagePayload::new(transaction, signer, relay)?;
        self.send_payload(pl).await
    }

    /// Forward a payload message by relay.
    /// It just create a new payload, cloned data, resigned with session and send
    async fn forward_by_relay(&self, payload: &MessagePayload, relay: MessageRelay) -> Result<()> {
        let new_pl =
            MessagePayload::new(payload.transaction.clone(), self.message_signer(), relay)?;
        self.send_payload(new_pl).await
    }

    /// Forward a payload message, with the next hop inferred by the DHT.
    async fn forward_payload(&self, payload: &MessagePayload, next_hop: Option<Did>) -> Result<()> {
        let next_hop = self.infer_next_hop(payload.relay.destination, next_hop)?;
        let relay = payload.relay.forward(self.dht().did, next_hop)?;
        self.forward_by_relay(payload, relay).await
    }

    /// Reset the destination to a secp DID.
    async fn reset_destination(&self, payload: &MessagePayload, next_hop: Did) -> Result<()> {
        let relay = payload
            .relay
            .reset_destination(next_hop)
            .forward(self.dht().did, next_hop)?;
        self.forward_by_relay(payload, relay).await
    }
}

#[cfg(test)]
pub mod test_payload;
