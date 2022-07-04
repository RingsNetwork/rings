use std::io::Write;

use async_trait::async_trait;
use flate2::write::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use web3::types::Address;

use super::encoder::Decoder;
use super::encoder::Encoded;
use super::encoder::Encoder;
use super::protocols::MessageRelay;
use super::protocols::MessageVerification;
use super::protocols::RelayMethod;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::ecc::PublicKey;
use crate::err::Error;
use crate::err::Result;
use crate::session::SessionManager;
use crate::utils;

const DEFAULT_TTL_MS: usize = 60 * 1000;

pub enum OriginVerificationGen {
    Origin,
    Stick(MessageVerification),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct MessagePayload<T> {
    pub data: T,
    pub tx_id: HashStr,
    pub addr: Address,
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
        let ts_ms = utils::get_epoch_ms();
        let ttl_ms = DEFAULT_TTL_MS;
        let msg = &MessageVerification::pack_msg(&data, ts_ms, ttl_ms)?;
        let tx_id = msg.into();
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
            vec![session_manager.authorizer()?.into()],
            None,
            Some(next_hop),
            destination,
        );
        Self::new(data, session_manager, OriginVerificationGen::Origin, relay)
    }

    pub fn new_report(
        data: T,
        session_manager: &SessionManager,
        relay: &MessageRelay,
    ) -> Result<Self> {
        let relay = relay.report()?;
        Self::new(data, session_manager, OriginVerificationGen::Origin, relay)
    }

    pub fn new_direct(data: T, session_manager: &SessionManager, destination: Did) -> Result<Self> {
        Self::new_send(data, session_manager, destination, destination)
    }

    pub fn is_expired(&self) -> bool {
        let now = utils::get_epoch_ms();
        now > self.verification.ts_ms + self.verification.ttl_ms as u128
            && now > self.origin_verification.ts_ms + self.origin_verification.ttl_ms as u128
    }

    pub fn verify(&self) -> bool {
        if self.is_expired() {
            return false;
        }

        self.verification.verify(&self.data) && self.origin_verification.verify(&self.data)
    }

    pub fn origin_session_pubkey(&self) -> Result<PublicKey> {
        self.origin_verification.session_pubkey(&self.data)
    }

    pub fn gzip(&self, level: u8) -> Result<Vec<u8>> {
        let mut ec = GzEncoder::new(Vec::new(), Compression::new(level as u32));
        let json_str = serde_json::to_string(self).map_err(|_| Error::SerializeToString)?;
        ec.write_all(json_str.as_bytes())
            .map_err(|_| Error::GzipEncode)?;
        ec.finish().map_err(|_| Error::GzipEncode)
    }

    pub fn from_gzipped(data: &[u8]) -> Result<Self>
    where T: DeserializeOwned {
        let mut writer = Vec::new();
        let mut decoder = GzDecoder::new(writer);
        decoder.write_all(data).map_err(|_| Error::GzipDecode)?;
        decoder.try_finish().map_err(|_| Error::GzipDecode)?;
        writer = decoder.finish().map_err(|_| Error::GzipDecode)?;
        let m = serde_json::from_slice(&writer).map_err(Error::Deserialize)?;
        Ok(m)
    }

    pub fn from_json(data: &[u8]) -> Result<Self> {
        serde_json::from_slice(data).map_err(Error::Deserialize)
    }

    pub fn to_json_vec(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(Error::Serialize)
    }

    pub fn from_auto(data: &[u8]) -> Result<Self> {
        if let Ok(m) = Self::from_gzipped(data) {
            return Ok(m);
        }
        Self::from_json(data)
    }
}

impl<T> Encoder for MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    fn encode(&self) -> Result<Encoded> {
        self.gzip(9)?.encode()
    }
}

impl<T> Decoder for MessagePayload<T>
where T: Serialize + DeserializeOwned
{
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Vec<u8> = encoded.decode()?;
        Self::from_auto(&v)
    }
}

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait PayloadSender<T>
where T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static
{
    fn session_manager(&self) -> &SessionManager;
    async fn do_send_payload(&self, address: &Address, payload: MessagePayload<T>) -> Result<()>;

    async fn send_payload(&self, payload: MessagePayload<T>) -> Result<()> {
        if let Some(id) = payload.relay.next_hop {
            self.do_send_payload(&id.into(), payload).await
        } else {
            Err(Error::NoNextHop)
        }
    }

    async fn send_message(&self, msg: T, next_hop: Did, destination: Did) -> Result<()> {
        self.send_payload(MessagePayload::new_send(
            msg,
            self.session_manager(),
            next_hop,
            destination,
        )?)
        .await
    }

    async fn send_direct_message(&self, msg: T, destination: Did) -> Result<()> {
        self.send_payload(MessagePayload::new_direct(
            msg,
            self.session_manager(),
            destination,
        )?)
        .await
    }

    async fn send_report_message(&self, msg: T, relay: MessageRelay) -> Result<()> {
        self.send_payload(MessagePayload::new_report(
            msg,
            self.session_manager(),
            &relay,
        )?)
        .await
    }

    async fn transpond_payload(
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
    use super::*;
    use crate::ecc::SecretKey;

    #[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
    pub struct TestData {
        a: String,
        b: i64,
        c: f64,
        d: bool,
    }

    pub fn new_test_payload() -> MessagePayload<TestData> {
        let key = SecretKey::random();
        let destination = SecretKey::random().address().into();
        let session = SessionManager::new_with_seckey(&key).unwrap();
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        MessagePayload::new_direct(test_data, &session, destination).unwrap()
    }

    #[test]
    fn new_then_verify() {
        let payload = new_test_payload();
        assert!(payload.verify());

        let key2 = SecretKey::random();
        let did2 = key2.address().into();
        let session2 = SessionManager::new_with_seckey(&key2).unwrap();

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
    fn test_message_relay_gzip() {
        let payload = new_test_payload();
        let gziped = payload.gzip(9).unwrap();
        let payload2: MessagePayload<TestData> = MessagePayload::from_gzipped(&gziped).unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_relay_from_auto() {
        let payload = new_test_payload();
        let gziped_encoded_payload = payload.encode().unwrap();
        let payload2: MessagePayload<TestData> = gziped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let ungzip_encoded_payload = payload.to_json_vec().unwrap().encode().unwrap();
        let payload2: MessagePayload<TestData> = ungzip_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);
    }
}
