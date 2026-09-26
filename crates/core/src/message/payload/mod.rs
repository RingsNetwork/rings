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
use crate::dht::delivery::NextHop;
use crate::dht::delivery::RouteStage;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::domain_tag;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;

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

/// The digest both signatures of a payload cover: every signed field of the transaction.
///
/// `reply_via` is encoded as a presence byte followed by the DID when present, before the
/// variable-length `data`, so the encoding stays injective.
fn hash_transaction(
    destination: Did,
    tx_id: uuid::Uuid,
    sequence: u64,
    reply_via: Option<Did>,
    data: &[u8],
) -> [u8; 32] {
    let mut msg = vec![];

    msg.extend_from_slice(destination.as_bytes());
    msg.extend_from_slice(tx_id.as_bytes());
    msg.extend_from_slice(&sequence.to_be_bytes());
    match reply_via {
        Some(peer) => {
            msg.push(1);
            msg.extend_from_slice(peer.as_bytes());
        }
        None => msg.push(0),
    }
    msg.extend_from_slice(data);

    keccak256(&msg)
}

/// All messages transmitted in RingsNetwork should be wrapped by `Transaction`.
/// It additionally offer destination, tx_id and verification.
///
/// A report for a transaction is routed to the transaction's [origin](Self::origin). Only the
/// successor and connection answers a node needs while it has no predecessor
/// (`FindSuccessorReport`, `ConnectNodeReport`) first go through
/// [`reply_via`](Self::reply_via) when the origin signed one. No other return address exists,
/// so a request can direct no report at a third party except one of these bounded answers at
/// the one peer the origin itself named, which receives it whole and hands it on only over a
/// direct link to the origin (see [`crate::dht::delivery`]).
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
    /// The peer the origin's successor and connection answers return through while no node is
    /// known to route to the origin: its nearest linked successor whenever it has no
    /// predecessor, i.e. while it joins and again after its predecessor departs until a new one
    /// notifies it; `None` otherwise. Signed with the rest of the transaction, so only the
    /// origin chooses it.
    pub reply_via: Option<Did>,
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
            .field("reply_via", &self.reply_via)
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
    /// Wrap data: serialize it with [rings_codec::serialize], then sign it, `reply_via`
    /// included, with `signer`. `reply_via` names the peer the origin's successor and
    /// connection answers return through; a report, or any transaction whose origin has a
    /// predecessor, names `None`.
    pub fn new<T>(
        destination: Did,
        tx_id: uuid::Uuid,
        sequence: u64,
        reply_via: Option<Did>,
        data: T,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let data = rings_codec::serialize(&data).map_err(Error::CodecSerialize)?;
        let msg_hash = hash_transaction(destination, tx_id, sequence, reply_via, &data);
        let verification = signer.sign(TRANSACTION_DOMAIN_TAG, &msg_hash)?;
        Ok(Self {
            destination,
            tx_id,
            sequence,
            reply_via,
            data,
            verification,
        })
    }

    /// The digest both the origin's and each hop's signature cover.
    fn signed_hash(&self) -> [u8; 32] {
        hash_transaction(
            self.destination,
            self.tx_id,
            self.sequence,
            self.reply_via,
            &self.data,
        )
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
        let verification = signer.sign(PAYLOAD_DOMAIN_TAG, &transaction.signed_hash())?;
        Ok(Self {
            transaction,
            relay,
            verification,
        })
    }

    /// A locally originated payload: a fresh carrier with the full hop budget leaving along
    /// the delivery decision `hop`, over a transaction that names `reply_via`.
    pub fn new_send_with_sequence<T>(
        data: T,
        signer: MessageSigner<&DelegateeKey>,
        hop: NextHop,
        destination: Did,
        sequence: u64,
        reply_via: Option<Did>,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let tx_id = crate::utils::new_uuid();
        let transaction = Transaction::new(destination, tx_id, sequence, reply_via, data, signer)?;
        let relay = MessageRelay::new(hop, transaction.destination, HopBudget::MAX);
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
        Self::new_send_with_sequence(
            data,
            signer,
            NextHop::toward(next_hop),
            destination,
            sequence,
            None,
        )
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
        Ok(self.signed_hash().to_vec())
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

    /// Whether `did` is a directly linked peer.
    fn is_connected(&self, did: Did) -> bool;

    /// Persistently reserve sender sequences for one final destination before signing.
    async fn reserve_transaction_sequences(
        &self,
        destination: Did,
        count: NonZeroU64,
    ) -> Result<std::ops::RangeInclusive<u64>>;

    /// Send a message payload to a specified DID.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()>;

    /// The delivery decision at this node for a payload addressed to the node `destination`
    /// whose carrier is in `stage`: [`delivery_step`](crate::dht::delivery::delivery_step) over
    /// this node's view and direct links. No hop passes its aim except by one marked handoff,
    /// and a route that cannot continue ends here with
    /// [`Error::RelayDestinationUnreachable`] instead of spending its hop budget.
    fn next_hop_toward(&self, destination: Did, stage: RouteStage) -> Result<NextHop> {
        self.dht()
            .delivery_step(destination, stage, |peer| self.is_connected(peer))?
            .ok_or(Error::RelayDestinationUnreachable { destination })
    }

    /// Build a locally originated payload for `destination`: the only constructor of a
    /// transaction this node authors, other than a report for a received request
    /// ([`Self::send_report_message`]).
    ///
    /// The first hop is `next_hop` when the caller fixed it, otherwise the delivery decision;
    /// the transaction names this node's [`reply_via`](crate::dht::delivery::reply_via). When
    /// the hop is decided here, both come from one topology snapshot
    /// ([`origination`](crate::dht::delivery::origination)). Every locally originated payload
    /// passes through here, so the law "a node without a predecessor names its nearest linked
    /// successor" holds for all of them. That includes a manually signalled connection answer,
    /// which names the hint harmlessly: no report is ever sent for it, and its only reader is
    /// the peer it is sent to.
    async fn originate<T>(
        &self,
        msg: T,
        destination: Did,
        next_hop: Option<Did>,
    ) -> Result<MessagePayload>
    where
        T: Serialize + Send,
    {
        let (hop, reply_via) = match next_hop {
            Some(peer) => (
                NextHop::toward(peer),
                self.dht().reply_via(|peer| self.is_connected(peer))?,
            ),
            None => {
                let origination = self
                    .dht()
                    .origination(destination, |peer| self.is_connected(peer))?;
                let hop = origination
                    .hop
                    .ok_or(Error::RelayDestinationUnreachable { destination })?;
                (hop, origination.reply_via)
            }
        };
        let sequence = *self
            .reserve_transaction_sequences(destination, NonZeroU64::MIN)
            .await?
            .start();
        MessagePayload::new_send_with_sequence(
            msg,
            self.message_signer(),
            hop,
            destination,
            sequence,
            reply_via,
        )
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
        let payload = self.originate(msg, destination, Some(next_hop)).await?;
        let tx_id = payload.transaction.tx_id;
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// Send a message to a specified destination.
    async fn send_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        let payload = self.originate(msg, destination, None).await?;
        let tx_id = payload.transaction.tx_id;
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// Send a direct message to a specified destination.
    async fn send_direct_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        self.send_message_by_hop(msg, destination, destination)
            .await
    }

    /// Send the report `msg` for the request carried by `payload`: a fresh payload routed to
    /// the request's origin under the same transaction id.
    ///
    /// A successor or connection answer (`FindSuccessorReport`, `ConnectNodeReport`) starts in
    /// stage `(via reply_via, ⊥)` when the signed request names one; every other report routes
    /// straight toward the origin, so a request can reflect at most one bounded report toward
    /// the peer it names. The first stage depends on the signed request alone, never on the
    /// request's carrier.
    async fn send_report_message(&self, payload: &MessagePayload, msg: Message) -> Result<()> {
        let origin = payload.transaction.origin();
        let reply_via = payload
            .transaction
            .reply_via
            .filter(|_| msg.returns_through_reply_via());
        let hop = self.next_hop_toward(origin, RouteStage::replying_via(reply_via))?;
        let relay = payload.relay.report(self.dht().did, origin, hop)?;

        let signer = self.message_signer();
        let sequence = *self
            .reserve_transaction_sequences(origin, NonZeroU64::MIN)
            .await?
            .start();
        let transaction = Transaction::new(
            origin,
            payload.transaction.tx_id,
            sequence,
            None,
            msg,
            signer,
        )?;

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

    /// Forward a payload one hop on toward its relay destination.
    ///
    /// With `next_hop = None` the hop is the delivery decision for the carrier's stage (see
    /// [`Self::next_hop_toward`]). A fixed `next_hop` is taken as given, as the owner-lookup
    /// protocols choose it from `find_successor`, unless the destination is directly linked;
    /// the stage is then carried unchanged.
    async fn forward_payload(&self, payload: &MessagePayload, next_hop: Option<Did>) -> Result<()> {
        let (local, destination) = (self.dht().did, payload.relay.destination);
        let hop = match next_hop {
            None => self.next_hop_toward(destination, payload.relay.stage)?,
            Some(_) if self.is_connected(destination) => {
                NextHop::new(destination, payload.relay.stage)
            }
            Some(next_hop) => NextHop::new(next_hop, payload.relay.stage),
        };
        let relay = payload.relay.forward(local, hop)?;
        self.forward_by_relay(payload, relay).await
    }

    /// Reset the destination to a secp DID.
    async fn reset_destination(&self, payload: &MessagePayload, next_hop: Did) -> Result<()> {
        let relay = payload
            .relay
            .reset_destination(next_hop)
            .forward(self.dht().did, NextHop::toward(next_hop))?;
        self.forward_by_relay(payload, relay).await
    }
}

#[cfg(test)]
pub mod test_payload;
