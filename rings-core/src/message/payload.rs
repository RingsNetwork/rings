use std::io::Write;

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
use super::protocols::RelayMethod;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::ecc::PublicKey;
use crate::err::Error;
use crate::err::Result;
use crate::session::SessionManager;
use crate::utils::get_epoch_ms;

pub fn encode_data_gzip(data: &Bytes, level: u8) -> Result<Bytes> {
    let mut ec = GzEncoder::new(Vec::new(), Compression::new(level as u32));
    tracing::info!("data before gzip len: {}", data.len());
    ec.write_all(data).map_err(|_| Error::GzipEncode)?;
    ec.finish().map(Bytes::from).map_err(|_| Error::GzipEncode)
}

pub fn gzip_data<T>(data: &T, level: u8) -> Result<Bytes>
where T: Serialize {
    let json_bytes = serde_json::to_vec(data).map_err(|_| Error::SerializeToString)?;
    encode_data_gzip(&json_bytes.into(), level)
}

pub fn decode_gzip_data(data: &Bytes) -> Result<Bytes> {
    let mut writer = Vec::new();
    let mut decoder = GzDecoder::new(writer);
    decoder.write_all(data).map_err(|_| Error::GzipDecode)?;
    decoder.try_finish().map_err(|_| Error::GzipDecode)?;
    writer = decoder.finish().map_err(|_| Error::GzipDecode)?;
    Ok(writer.into())
}

pub fn from_gzipped_data<T>(data: &Bytes) -> Result<T>
where T: DeserializeOwned {
    let data = decode_gzip_data(data)?;
    let m = serde_json::from_slice(&data).map_err(Error::Deserialize)?;
    Ok(m)
}

pub enum OriginVerificationGen {
    Origin,
    Stick(MessageVerification),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
/// MessagePayload with sequence and verification, contain MessageRealy.
pub struct MessagePayload<T> {
    pub data: T,
    pub tx_id: uuid::Uuid,
    pub addr: Did,
    pub verification: MessageVerification,
    pub origin_verification: MessageVerification,
    pub relay: MessageRelay,
}

impl<T> MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    pub fn new(
        data: T,
        session_manager: &SessionManager,
        origin_verification_gen: OriginVerificationGen,
        relay: MessageRelay,
    ) -> Result<Self> {
        let ts_ms = get_epoch_ms();
        let ttl_ms = DEFAULT_TTL_MS;
        let msg = &MessageVerification::pack_msg(&data, ts_ms, ttl_ms)?;
        let tx_id = uuid::Uuid::new_v4();
        let addr = session_manager.authorizer()?;
        let verification = MessageVerification {
            session: session_manager.session()?,
            sig: session_manager.sign(msg)?,
            ttl_ms,
            ts_ms,
        };

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

    pub fn new_send(
        data: T,
        session_manager: &SessionManager,
        next_hop: Did,
        destination: Did,
    ) -> Result<Self> {
        let relay = MessageRelay::new(
            RelayMethod::SEND,
            vec![session_manager.authorizer()?],
            None,
            Some(next_hop),
            destination,
        );
        Self::new(data, session_manager, OriginVerificationGen::Origin, relay)
    }

    pub fn new_report(
        data: T,
        tx_id: uuid::Uuid,
        session_manager: &SessionManager,
        relay: &MessageRelay,
    ) -> Result<Self> {
        let relay = relay.report()?;
        let mut pl = Self::new(data, session_manager, OriginVerificationGen::Origin, relay)?;
        pl.tx_id = tx_id;
        Ok(pl)
    }

    /// new_direct is A specific new_send, with same next_hop and destination
    /// just like a normal server-client base model.
    pub fn new_direct(data: T, session_manager: &SessionManager, destination: Did) -> Result<Self> {
        Self::new_send(data, session_manager, destination, destination)
    }

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

    pub fn verify(&self) -> bool {
        tracing::debug!("verifying payload: {:?}", self.tx_id);

        if self.is_expired() {
            tracing::warn!("message expired");
            return false;
        }

        self.verification.verify(&self.data) && self.origin_verification.verify(&self.data)
    }

    pub fn origin_session_pubkey(&self) -> Result<PublicKey> {
        self.origin_verification.session_pubkey(&self.data)
    }

    pub fn from_bincode(data: &[u8]) -> Result<Self> {
        bincode::deserialize(data).map_err(Error::BincodeDeserialize)
    }

    pub fn to_bincode(&self) -> Result<Bytes> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(Error::BincodeSerialize)
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait PayloadSender<T>
where T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static
{
    fn session_manager(&self) -> &SessionManager;
    async fn do_send_payload(&self, did: Did, payload: MessagePayload<T>) -> Result<()>;

    async fn send_payload(&self, payload: MessagePayload<T>) -> Result<()> {
        if let Some(did) = payload.relay.next_hop {
            self.do_send_payload(did, payload).await
        } else {
            Err(Error::NoNextHop)
        }
    }

    async fn send_message(&self, msg: T, next_hop: Did, destination: Did) -> Result<uuid::Uuid> {
        let payload = MessagePayload::new_send(msg, self.session_manager(), next_hop, destination)?;
        self.send_payload(payload.clone()).await?;
        Ok(payload.tx_id)
    }

    async fn send_direct_message(&self, msg: T, destination: Did) -> Result<uuid::Uuid> {
        let payload = MessagePayload::new_direct(msg, self.session_manager(), destination)?;
        self.send_payload(payload.clone()).await?;
        Ok(payload.tx_id)
    }

    async fn send_report_message(
        &self,
        msg: T,
        tx_id: uuid::Uuid,
        relay: MessageRelay,
    ) -> Result<()> {
        self.send_payload(MessagePayload::new_report(
            msg,
            tx_id,
            self.session_manager(),
            &relay,
        )?)
        .await
    }

    async fn forward_payload(
        &self,
        payload: &MessagePayload<T>,
        relay: MessageRelay,
    ) -> Result<()> {
        self.send_payload(MessagePayload::new(
            payload.data.clone(),
            self.session_manager(),
            OriginVerificationGen::Stick(payload.origin_verification.clone()),
            relay,
        )?)
        .await
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

    pub fn new_test_payload() -> MessagePayload<TestData> {
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        new_payload(test_data)
    }

    pub fn new_payload<T>(data: T) -> MessagePayload<T>
    where T: Serialize + DeserializeOwned {
        let key = SecretKey::random();
        let destination = SecretKey::random().address().into();
        let session = SessionManager::new_with_seckey(&key, None).unwrap();
        MessagePayload::new_direct(data, &session, destination).unwrap()
    }

    #[test]
    fn new_then_verify() {
        let payload = new_test_payload();
        assert!(payload.verify());

        let key2 = SecretKey::random();
        let did2 = key2.address().into();
        let session2 = SessionManager::new_with_seckey(&key2, None).unwrap();

        let mut relay = payload.relay.clone();
        relay.next_hop = Some(did2);
        relay.relay(did2, None).unwrap();

        let relaied_payload = MessagePayload::new(
            payload.data.clone(),
            &session2,
            OriginVerificationGen::Stick(payload.origin_verification),
            relay,
        )
        .unwrap();

        assert!(relaied_payload.verify());
    }

    #[test]
    fn test_message_payload_from_auto() {
        let payload = new_test_payload();
        let gziped_encoded_payload = payload.encode().unwrap();
        let payload2: MessagePayload<TestData> = gziped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let gunzip_encoded_payload = payload.to_bincode().unwrap().encode().unwrap();
        let payload2: MessagePayload<TestData> = gunzip_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_payload_encode_len() {
        let data = rand::thread_rng().gen::<[u8; 32]>();

        let data1 = data;
        let msg1 = Message::custom(&data1, None).unwrap();
        let payload1 = new_payload(msg1);
        let bytes1 = payload1.to_bincode().unwrap();
        let encoded1 = payload1.encode().unwrap();
        let encoded_bytes1: Vec<u8> = encoded1.into();

        let data2 = data.repeat(2);
        let msg2 = Message::custom(&data2, None).unwrap();
        let payload2 = new_payload(msg2);
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
