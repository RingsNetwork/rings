#![deny(missing_docs)]

use std::fmt;
use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use flate2::write::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;

use super::encoder::Decoder;
use super::encoder::Encoded;
use super::encoder::Encoder;
use super::protocols::MessageRelay;
use super::protocols::MessageVerification;
use super::protocols::MessageVerificationExt;
use super::protocols::ReportReturnPolicy;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;
use crate::session::SessionSk;

/// Compresses the given data byte slice using the gzip algorithm with the specified compression level.
pub fn encode_data_gzip(data: &Bytes, level: u8) -> Result<Bytes> {
    let mut ec = GzEncoder::new(Vec::new(), Compression::new(level as u32));
    ec.write_all(data).map_err(|_| Error::GzipEncode)?;
    ec.finish().map(Bytes::from).map_err(|_| Error::GzipEncode)
}

/// Serializes the given data using JSON and compresses it with gzip using the specified compression level.
pub fn gzip_data<T>(data: &T, level: u8) -> Result<Bytes>
where T: Serialize {
    let json_bytes = serde_json::to_vec(data).map_err(|_| Error::SerializeToString)?;
    encode_data_gzip(&json_bytes.into(), level)
}

/// Decompresses the given gzip-compressed byte slice and returns the decompressed byte slice.
pub fn decode_gzip_data(data: &Bytes) -> Result<Bytes> {
    let mut writer = Vec::new();
    let mut decoder = GzDecoder::new(writer);
    decoder.write_all(data).map_err(|_| Error::GzipDecode)?;
    decoder.try_finish().map_err(|_| Error::GzipDecode)?;
    writer = decoder.finish().map_err(|_| Error::GzipDecode)?;
    Ok(writer.into())
}

/// From gzip data to deserialized
pub fn from_gzipped_data<T>(data: &Bytes) -> Result<T>
where T: DeserializeOwned {
    let data = decode_gzip_data(data)?;
    let m = serde_json::from_slice(&data).map_err(Error::Deserialize)?;
    Ok(m)
}

fn hash_transaction(
    destination: Did,
    tx_id: uuid::Uuid,
    report_return: ReportReturnPolicy,
    data: &[u8],
) -> [u8; 32] {
    let mut msg = vec![];

    msg.extend_from_slice(destination.as_bytes());
    msg.extend_from_slice(tx_id.as_bytes());
    match report_return {
        ReportReturnPolicy::Path => msg.push(0),
        ReportReturnPolicy::Routed { destination } => {
            msg.push(1);
            msg.extend_from_slice(destination.as_bytes());
        }
    }
    msg.extend_from_slice(data);

    keccak256(&msg)
}

/// All messages transmitted in RingsNetwork should be wrapped by `Transaction`.
/// It additionally offer destination, tx_id and verification.
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
    /// data
    pub data: Vec<u8>,
    /// Return policy used by reports for this transaction.
    #[serde(default)]
    pub report_return: ReportReturnPolicy,
    /// This field holds a signature from a node,
    /// which is used to prove that the transaction was created by that node.
    pub verification: MessageVerification,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("destination", &self.destination)
            .field("tx_id", &self.tx_id)
            .field("data_bytes", &self.data.len())
            .field("report_return", &self.report_return)
            .finish()
    }
}

/// `MessagePayload` is used to transmit data between nodes.
/// The data should be packed by [Transaction].
#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct MessagePayload {
    /// Payload data
    pub transaction: Transaction,
    /// Relay records the transport path of message.
    /// And can also help message sender to find the next hop.
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
    /// then sign [MessageVerification] by session_sk.
    pub fn new<T>(
        destination: Did,
        tx_id: uuid::Uuid,
        data: T,
        session_sk: &SessionSk,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        Self::new_with_report_return(
            destination,
            tx_id,
            data,
            ReportReturnPolicy::Path,
            session_sk,
        )
    }

    /// Wrap data with an explicit report-return policy.
    pub fn new_with_report_return<T>(
        destination: Did,
        tx_id: uuid::Uuid,
        data: T,
        report_return: ReportReturnPolicy,
        session_sk: &SessionSk,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        report_return.validate_authorized_by(session_sk.account_did())?;
        let data = rings_codec::serialize(&data).map_err(Error::CodecSerialize)?;
        let msg_hash = hash_transaction(destination, tx_id, report_return, &data);
        let verification = MessageVerification::new(&msg_hash, session_sk)?;
        Ok(Self {
            destination,
            tx_id,
            data,
            report_return,
            verification,
        })
    }

    /// Deserializes the data field into a `T` instance.
    pub fn data<T>(&self) -> Result<T>
    where T: DeserializeOwned {
        rings_codec::deserialize(&self.data).map_err(Error::CodecDeserialize)
    }
}

impl MessagePayload {
    /// Create new `MessagePayload`.
    /// Need [Transaction], [SessionSk] and [MessageRelay].
    pub fn new(
        transaction: Transaction,
        session_sk: &SessionSk,
        relay: MessageRelay,
    ) -> Result<Self> {
        let msg_hash = hash_transaction(
            transaction.destination,
            transaction.tx_id,
            transaction.report_return,
            &transaction.data,
        );
        let verification = MessageVerification::new(&msg_hash, session_sk)?;
        Ok(Self {
            transaction,
            relay,
            verification,
        })
    }

    /// Helps to create sending message from data.
    pub fn new_send<T>(
        data: T,
        session_sk: &SessionSk,
        next_hop: Did,
        destination: Did,
    ) -> Result<Self>
    where
        T: Serialize,
    {
        let tx_id = uuid::Uuid::new_v4();
        let transaction = Transaction::new(destination, tx_id, data, session_sk)?;
        let relay = MessageRelay::new(
            vec![session_sk.account_did()],
            next_hop,
            transaction.destination,
        );
        Self::new(transaction, session_sk, relay)
    }

    /// Deserializes a `MessagePayload` instance from the Rings wire encoding.
    pub fn from_wire(data: &[u8]) -> Result<Self> {
        rings_codec::deserialize(data).map_err(Error::CodecDeserialize)
    }

    /// Serializes the `MessagePayload` instance into the Rings wire encoding.
    pub fn to_wire(&self) -> Result<Bytes> {
        rings_codec::serialize(self)
            .map(Bytes::from)
            .map_err(Error::CodecSerialize)
    }

    /// Return the exact Rings wire size without allocating the wire buffer.
    pub(crate) fn wire_size(&self) -> Result<usize> {
        let bytes = rings_codec::serialized_size(self).map_err(Error::CodecSerialize)?;
        usize::try_from(bytes).map_err(|_| Error::MessageSizeOverflow)
    }

    /// Returns whether `local` is the relay destination of this payload.
    pub(crate) fn is_relay_destination_for(&self, local: Did) -> bool {
        self.relay.destination == local
    }

    /// Returns whether `local` should forward this payload to another node.
    pub(crate) fn should_forward_from(&self, local: Did) -> bool {
        !self.is_relay_destination_for(local)
    }
}

impl MessageVerificationExt for Transaction {
    fn verification_data(&self) -> Result<Vec<u8>> {
        self.report_return.validate_authorized_by(self.signer())?;
        Ok(hash_transaction(self.destination, self.tx_id, self.report_return, &self.data).to_vec())
    }

    fn verification(&self) -> &MessageVerification {
        &self.verification
    }
}

impl MessageVerificationExt for MessagePayload {
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
    /// Get the session sk
    fn session_sk(&self) -> &SessionSk;

    /// Get access to DHT.
    fn dht(&self) -> Arc<PeerRing>;

    /// Used to check if destination is already connected when `infer_next_hop`
    fn is_connected(&self, did: Did) -> bool;

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
        let payload = MessagePayload::new_send(msg, self.session_sk(), next_hop, destination)?;
        let tx_id = payload.transaction.tx_id;
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// Send a message to a specified destination by specified next hop with an explicit report policy.
    async fn send_message_by_hop_with_report_return<T>(
        &self,
        msg: T,
        destination: Did,
        next_hop: Did,
        report_return: ReportReturnPolicy,
    ) -> Result<uuid::Uuid>
    where
        T: Serialize + Send,
    {
        let tx_id = uuid::Uuid::new_v4();
        let transaction = Transaction::new_with_report_return(
            destination,
            tx_id,
            msg,
            report_return,
            self.session_sk(),
        )?;
        let relay = MessageRelay::new(
            vec![self.session_sk().account_did()],
            next_hop,
            transaction.destination,
        );
        let payload = MessagePayload::new(transaction, self.session_sk(), relay)?;
        self.send_payload(payload).await?;
        Ok(tx_id)
    }

    /// Send a message to a specified destination.
    async fn send_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        let next_hop = self.infer_next_hop(destination, None)?;
        self.send_message_by_hop(msg, destination, next_hop).await
    }

    /// Send a message to a specified destination with an explicit report policy.
    async fn send_message_with_report_return<T>(
        &self,
        msg: T,
        destination: Did,
        report_return: ReportReturnPolicy,
    ) -> Result<uuid::Uuid>
    where
        T: Serialize + Send,
    {
        let next_hop = self.infer_next_hop(destination, None)?;
        self.send_message_by_hop_with_report_return(msg, destination, next_hop, report_return)
            .await
    }

    /// Send a direct message to a specified destination.
    async fn send_direct_message<T>(&self, msg: T, destination: Did) -> Result<uuid::Uuid>
    where T: Serialize + Send {
        self.send_message_by_hop(msg, destination, destination)
            .await
    }

    /// Send a report message to a specified destination.
    async fn send_report_message<T>(&self, payload: &MessagePayload, msg: T) -> Result<()>
    where T: Serialize + Send {
        let policy = payload.transaction.report_return;
        // Keep this send-boundary check even though transaction verification
        // enforces the same authorization when the request is received.
        policy.validate_authorized_by(payload.transaction.signer())?;
        let routed_next_hop = match policy {
            ReportReturnPolicy::Path => None,
            ReportReturnPolicy::Routed { destination } => {
                Some(self.infer_next_hop(destination, None)?)
            }
        };
        let relay = payload
            .relay
            .report(self.dht().did, policy, routed_next_hop)?;

        let transaction = Transaction::new(
            relay.destination,
            payload.transaction.tx_id,
            msg,
            self.session_sk(),
        )?;

        let pl = MessagePayload::new(transaction, self.session_sk(), relay)?;
        self.send_payload(pl).await
    }

    /// Forward a payload message by relay.
    /// It just create a new payload, cloned data, resigned with session and send
    async fn forward_by_relay(&self, payload: &MessagePayload, relay: MessageRelay) -> Result<()> {
        let new_pl = MessagePayload::new(payload.transaction.clone(), self.session_sk(), relay)?;
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
