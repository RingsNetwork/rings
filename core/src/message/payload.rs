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
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::error::Error;
use crate::error::Result;
use crate::session::DelegateeSk;
use crate::utils::get_epoch_ms;

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

/// An enumeration of options for generating origin verification or stick verification.
/// Verification can be Stick Verification or origin verification.
/// When MessagePayload created, Origin Verification is always generated.
/// and if OriginVerificationGen is stick, it can including existing stick ov
pub enum OriginVerificationGen {
    Origin,
    Stick(MessageVerification),
}

/// All messages transmitted in RingsNetwork should be wrapped by MessagePayload.
/// It additionally offer transaction ID, origin did, relay, previous hop verification,
/// and origin verification.
#[derive(Derivative, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[derivative(Debug)]
pub struct MessagePayload<T> {
    /// Payload data
    pub data: T,
    /// The transaction ID of payload.
    /// Remote peer should use same tx_id when response.
    pub tx_id: uuid::Uuid,
    /// The did of payload authorizer, usually it's last sender.
    pub addr: Did,
    /// Relay records the transport path of message.
    /// And can also help message sender to find the next hop.
    pub relay: MessageRelay,
    /// This field hold a signature from a node,
    /// which is used to prove that the message was sent from that node.
    #[derivative(Debug = "ignore")]
    pub verification: MessageVerification,
    /// Same as verification, but the signature was from the original sender.
    #[derivative(Debug = "ignore")]
    pub origin_verification: MessageVerification,
}

impl<T> MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    /// Create new instance
    pub fn new(
        data: T,
        delegatee_sk: &DelegateeSk,
        origin_verification_gen: OriginVerificationGen,
        relay: MessageRelay,
    ) -> Result<Self> {
        let ts_ms = get_epoch_ms();
        let ttl_ms = DEFAULT_TTL_MS;
        let msg = &MessageVerification::pack_msg(&data, ts_ms, ttl_ms)?;
        let tx_id = uuid::Uuid::new_v4();
        let addr = delegatee_sk.authorizer_did();
        let verification = MessageVerification {
            session: delegatee_sk.session(),
            sig: delegatee_sk.sign(msg)?,
            ttl_ms,
            ts_ms,
        };
        // If origin_verification_gen is set to Origin, simply clone it into.
        let origin_verification = match origin_verification_gen {
            OriginVerificationGen::Origin => verification.clone(),
            OriginVerificationGen::Stick(ov) => ov,
        };

        Ok(Self {
            data,
            tx_id,
            addr,
            verification,
            origin_verification,
            relay,
        })
    }

    /// Create new Payload for send
    pub fn new_send(
        data: T,
        delegatee_sk: &DelegateeSk,
        next_hop: Did,
        destination: Did,
    ) -> Result<Self> {
        let relay = MessageRelay::new(vec![delegatee_sk.authorizer_did()], next_hop, destination);
        Self::new(data, delegatee_sk, OriginVerificationGen::Origin, relay)
    }

    /// Checks whether the payload is expired.
    pub fn is_expired(&self) -> bool {
        if self.verification.ttl_ms > MAX_TTL_MS {
            return false;
        }

        if self.origin_verification.ttl_ms > MAX_TTL_MS {
            return false;
        }

        let now = get_epoch_ms();

        if self.verification.ts_ms - TS_OFFSET_TOLERANCE_MS > now {
            return false;
        }

        if self.origin_verification.ts_ms - TS_OFFSET_TOLERANCE_MS > now {
            return false;
        }

        now > self.verification.ts_ms + self.verification.ttl_ms as u128
            && now > self.origin_verification.ts_ms + self.origin_verification.ttl_ms as u128
    }

    /// Verifies that the payload is not expired and that the signature is valid.
    pub fn verify(&self) -> bool {
        tracing::debug!("verifying payload: {:?}", self.tx_id);

        if self.is_expired() {
            tracing::warn!("message expired");
            return false;
        }

        if Some(self.relay.origin_sender()) != self.origin_authorizer_did().ok() {
            tracing::warn!("sender is not origin_verification generator");
            return false;
        }

        self.verification.verify(&self.data) && self.origin_verification.verify(&self.data)
    }

    /// Get Did from the origin verification.
    pub fn origin_authorizer_did(&self) -> Result<Did> {
        Ok(self
            .origin_verification
            .session
            .authorizer_pubkey()?
            .address()
            .into())
    }

    /// Get did from sender verification.
    pub fn authorizer_did(&self) -> Result<Did> {
        Ok(self
            .verification
            .session
            .authorizer_pubkey()?
            .address()
            .into())
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

    /// Did of Sender
    pub fn sender(&self) -> Result<Did> {
        self.authorizer_did()
    }

    /// Did of Origin
    pub fn origin(&self) -> Result<Did> {
        self.origin_authorizer_did()
    }
}

impl<T> Encoder for MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    fn encode(&self) -> Result<Encoded> {
        self.to_bincode()?.encode()
    }
}

impl<T> Decoder for MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Bytes = encoded.decode()?;
        Self::from_bincode(&v)
    }
}

/// Trait of PayloadSender
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait PayloadSender<T>
where T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static
{
    /// Get the delegatee sk
    fn delegatee_sk(&self) -> &DelegateeSk;
    /// Get access to DHT.
    fn dht(&self) -> Arc<PeerRing>;
    /// Send a message payload to a specified DID.
    async fn do_send_payload(&self, did: Did, payload: MessagePayload<T>) -> Result<()>;
    /// Infer the next hop for a message by calling `dht.find_successor()`.
    fn infer_next_hop(&self, next_hop: Option<Did>, destination: Did) -> Result<Did> {
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
    async fn send_payload(&self, payload: MessagePayload<T>) -> Result<()> {
        self.do_send_payload(payload.relay.next_hop, payload).await
    }

    /// Send a message to a specified destination.
    async fn send_message(&self, msg: T, destination: Did) -> Result<uuid::Uuid> {
        let next_hop = self.infer_next_hop(None, destination)?;
        let payload = MessagePayload::new_send(msg, self.delegatee_sk(), next_hop, destination)?;
        self.send_payload(payload.clone()).await?;
        Ok(payload.tx_id)
    }

    /// Send a message to a specified destination by specified next hop.
    async fn send_message_by_hop(
        &self,
        msg: T,
        destination: Did,
        next_hop: Did,
    ) -> Result<uuid::Uuid> {
        let payload = MessagePayload::new_send(msg, self.delegatee_sk(), next_hop, destination)?;
        self.send_payload(payload.clone()).await?;
        Ok(payload.tx_id)
    }

    /// Send a direct message to a specified destination.
    async fn send_direct_message(&self, msg: T, destination: Did) -> Result<uuid::Uuid> {
        let payload = MessagePayload::new_send(msg, self.delegatee_sk(), destination, destination)?;
        self.send_payload(payload.clone()).await?;
        Ok(payload.tx_id)
    }

    /// Send a report message to a specified destination.
    async fn send_report_message(&self, payload: &MessagePayload<T>, msg: T) -> Result<()> {
        let relay = payload.relay.report(self.dht().did)?;

        let mut pl = MessagePayload::new(
            msg,
            self.delegatee_sk(),
            OriginVerificationGen::Origin,
            relay,
        )?;
        pl.tx_id = payload.tx_id;

        self.send_payload(pl).await
    }

    /// Forward a payload message by relay.
    /// It just create a new payload, cloned data, resigned with session and send
    async fn forward_by_relay(
        &self,
        payload: &MessagePayload<T>,
        relay: MessageRelay,
    ) -> Result<()> {
        let mut new_pl = MessagePayload::new(
            payload.data.clone(),
            self.delegatee_sk(),
            OriginVerificationGen::Stick(payload.origin_verification.clone()),
            relay,
        )?;
        new_pl.tx_id = payload.tx_id;
        self.send_payload(new_pl).await
    }

    /// Forward a payload message, with the next hop inferred by the DHT.
    async fn forward_payload(
        &self,
        payload: &MessagePayload<T>,
        next_hop: Option<Did>,
    ) -> Result<()> {
        let next_hop = self.infer_next_hop(next_hop, payload.relay.destination)?;
        let relay = payload.relay.forward(self.dht().did, next_hop)?;
        self.forward_by_relay(payload, relay).await
    }

    /// Reset the destination to a secp DID.
    async fn reset_destination(&self, payload: &MessagePayload<T>, next_hop: Did) -> Result<()> {
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

    pub fn new_test_payload(next_hop: Did) -> MessagePayload<TestData> {
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        new_payload(test_data, next_hop)
    }

    pub fn new_payload<T>(data: T, next_hop: Did) -> MessagePayload<T>
    where T: Serialize + DeserializeOwned {
        let key = SecretKey::random();
        let destination = SecretKey::random().address().into();
        let session = DelegateeSk::new_with_seckey(&key).unwrap();
        MessagePayload::new_send(data, &session, next_hop, destination).unwrap()
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
        let payload2: MessagePayload<TestData> = gzipped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let gunzip_encoded_payload = payload.to_bincode().unwrap().encode().unwrap();
        let payload2: MessagePayload<TestData> = gunzip_encoded_payload.decode().unwrap();
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
