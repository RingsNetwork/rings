#![warn(missing_docs)]

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use derivative::Derivative;
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
    tracing::info!("data before gzip len: {}", data.len());
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

fn hash_transaction(destination: Did, tx_id: uuid::Uuid, data: &[u8]) -> [u8; 32] {
    let mut msg = vec![];

    msg.extend_from_slice(destination.as_bytes());
    msg.extend_from_slice(tx_id.as_bytes());
    msg.extend_from_slice(data);

    keccak256(&msg)
}

/// All messages transmitted in RingsNetwork should be wrapped by `Transaction`.
/// It additionally offer destination, tx_id and verification.
///
/// To transmit `Transaction` in RingsNetwork, user should build
/// [MessagePayload] and use [PayloadSender] to send.
#[derive(Derivative, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[derivative(Debug)]
pub struct Transaction {
    /// The destination of this message.
    pub destination: Did,
    /// The transaction ID.
    /// Remote peer should use same tx_id when response.
    pub tx_id: uuid::Uuid,
    /// data
    pub data: Vec<u8>,
    /// This field holds a signature from a node,
    /// which is used to prove that the transaction was created by that node.
    #[derivative(Debug = "ignore")]
    pub verification: MessageVerification,
}

/// `MessagePayload` is used to transmit data between nodes.
/// The data should be packed by [Transaction].
#[derive(Derivative, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[derivative(Debug)]
pub struct MessagePayload {
    /// Payload data
    pub transaction: Transaction,
    /// Relay records the transport path of message.
    /// And can also help message sender to find the next hop.
    pub relay: MessageRelay,
    /// This field holds a signature from a node,
    /// which is used to prove that payload was created by that node.
    #[derivative(Debug = "ignore")]
    pub verification: MessageVerification,
}

impl Transaction {
    /// Wrap data. Will serialize by [bincode::serialize]
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
        let data = bincode::serialize(&data).map_err(Error::BincodeSerialize)?;
        let msg_hash = hash_transaction(destination, tx_id, &data);
        let verification = MessageVerification::new(&msg_hash, session_sk)?;
        Ok(Self {
            destination,
            tx_id,
            data,
            verification,
        })
    }

    /// Deserializes the data field into a `T` instance.
    pub fn data<T>(&self) -> Result<T>
    where T: DeserializeOwned {
        bincode::deserialize(&self.data).map_err(Error::BincodeDeserialize)
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

    /// Deserializes a `MessagePayload` instance from the given binary data.
    pub fn from_bincode(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(Error::BincodeDeserialize)
    }

    /// Serializes the `MessagePayload` instance into binary data.
    pub fn to_bincode(&self) -> Result<Bytes> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(Error::BincodeSerialize)
    }
}

impl MessageVerificationExt for Transaction {
    fn verification_data(&self) -> Result<Vec<u8>> {
        Ok(hash_transaction(self.destination, self.tx_id, &self.data).to_vec())
    }

    fn verification(&self) -> &MessageVerification {
        &self.verification
    }
}

impl MessageVerificationExt for MessagePayload {
    fn verification_data(&self) -> Result<Vec<u8>> {
        Ok(hash_transaction(
            self.transaction.destination,
            self.transaction.tx_id,
            &self.transaction.data,
        )
        .to_vec())
    }

    fn verification(&self) -> &MessageVerification {
        &self.verification
    }
}

impl Encoder for MessagePayload {
    fn encode(&self) -> Result<Encoded> {
        self.to_bincode()?.encode()
    }
}

impl Decoder for MessagePayload {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Bytes = encoded.decode()?;
        Self::from_bincode(&v)
    }
}

/// Trait of PayloadSender
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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

    /// Send a report message to a specified destination.
    async fn send_report_message<T>(&self, payload: &MessagePayload, msg: T) -> Result<()>
    where T: Serialize + Send {
        let relay = payload.relay.report(self.dht().did)?;

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
pub mod test {
    use rand::Rng;

    use super::*;
    use crate::ecc::SecretKey;
    use crate::message::Message;

    #[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
    pub struct TestData {
        a: String,
        b: i64,
        c: f64,
        d: bool,
    }

    pub fn new_test_payload(next_hop: Did) -> MessagePayload {
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        new_payload(test_data, next_hop)
    }

    pub fn new_payload<T>(data: T, next_hop: Did) -> MessagePayload
    where T: Serialize + DeserializeOwned {
        let key = SecretKey::random();
        let destination = SecretKey::random().address().into();
        let session_sk = SessionSk::new_with_seckey(&key).unwrap();
        MessagePayload::new_send(data, &session_sk, next_hop, destination).unwrap()
    }

    #[test]
    fn new_then_verify() {
        let key2 = SecretKey::random();
        let did2 = key2.address().into();

        let payload = new_test_payload(did2);
        assert!(payload.verify());
    }

    #[test]
    fn test_message_payload_from_auto() {
        let next_hop = SecretKey::random().address().into();

        let payload = new_test_payload(next_hop);
        let gzipped_encoded_payload = payload.encode().unwrap();
        let payload2: MessagePayload = gzipped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let gunzip_encoded_payload = payload.to_bincode().unwrap().encode().unwrap();
        let payload2: MessagePayload = gunzip_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_payload_encode_len() {
        let next_hop = SecretKey::random().address().into();
        let data = rand::thread_rng().gen::<[u8; 32]>();

        let data1 = data;
        let msg1 = Message::custom(&data1).unwrap();
        let payload1 = new_payload(msg1, next_hop);
        let bytes1 = payload1.to_bincode().unwrap();
        let encoded1 = payload1.encode().unwrap();
        let encoded_bytes1: Vec<u8> = encoded1.into();

        let data2 = data.repeat(2);
        let msg2 = Message::custom(&data2).unwrap();
        let payload2 = new_payload(msg2, next_hop);
        let bytes2 = payload2.to_bincode().unwrap();
        let encoded2 = payload2.encode().unwrap();
        let encoded_bytes2: Vec<u8> = encoded2.into();

        assert_eq!(bytes1.len() - data1.len(), bytes2.len() - data2.len());
        assert_ne!(
            encoded_bytes1.len() - data1.len(),
            encoded_bytes2.len() - data2.len()
        );
    }
}
