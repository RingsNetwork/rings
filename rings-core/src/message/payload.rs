use crate::dht::Did;
use crate::ecc::{signers, HashStr, PublicKey};
use crate::err::{Error, Result};
use crate::session::SessionManager;
use crate::session::{Session, Signer};
use crate::utils;
use flate2::write::{GzDecoder, GzEncoder};
use flate2::Compression;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::collections::VecDeque;
use std::io::Write;
use web3::types::Address;

use super::encoder::{Decoder, Encoded, Encoder};

const DEFAULT_TTL_MS: usize = 60 * 1000;

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum MessageRelayMethod {
    SEND,
    REPORT,
}

pub enum OriginVerificationGen {
    Origin,
    Stick(MessageVerification),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct MessageVerification {
    pub session: Session,
    pub ttl_ms: usize,
    pub ts_ms: u128,
    pub sig: Vec<u8>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct MessageRelay<T> {
    pub data: T,
    pub tx_id: HashStr,
    pub to_path: VecDeque<Did>,
    pub from_path: VecDeque<Did>,
    pub addr: Address,
    pub relay_verification: MessageVerification,
    pub origin_verification: MessageVerification,
    pub method: MessageRelayMethod,
}

impl MessageVerification {
    pub fn verify<T>(&self, data: &T) -> bool
    where
        T: Serialize,
    {
        if !self.session.verify() {
            return false;
        }

        if let (Ok(addr), Ok(msg)) = (self.session.address(), self.msg(data)) {
            match self.session.auth.signer {
                Signer::DEFAULT => signers::default::verify(&msg, &addr, &self.sig),
                Signer::EIP712 => signers::eip712::verify(&msg, &addr, &self.sig),
            }
        } else {
            false
        }
    }

    pub fn session_pubkey<T>(&self, data: &T) -> Result<PublicKey>
    where
        T: Serialize,
    {
        let msg = self.msg(data)?;
        match self.session.auth.signer {
            Signer::DEFAULT => signers::default::recover(&msg, &self.sig),
            Signer::EIP712 => signers::eip712::recover(&msg, &self.sig),
        }
    }

    pub fn pack_msg<T>(data: &T, ts_ms: u128, ttl_ms: usize) -> Result<String>
    where
        T: Serialize,
    {
        let mut msg = serde_json::to_string(data).map_err(|_| Error::SerializeToString)?;
        msg.push_str(&format!("\n{}\n{}", ts_ms, ttl_ms));
        Ok(msg)
    }

    fn msg<T>(&self, data: &T) -> Result<String>
    where
        T: Serialize,
    {
        Self::pack_msg(data, self.ts_ms, self.ttl_ms)
    }
}

impl<T> MessageRelay<T>
where
    T: Serialize + DeserializeOwned,
{
    pub fn new(
        data: T,
        session_manager: &SessionManager,
        origin_verification_gen: OriginVerificationGen,
        ttl_ms: Option<usize>,
        to_path: Option<VecDeque<Did>>,
        from_path: Option<VecDeque<Did>>,
        method: MessageRelayMethod,
    ) -> Result<Self> {
        let ts_ms = utils::get_epoch_ms();
        let ttl_ms = ttl_ms.unwrap_or(DEFAULT_TTL_MS);
        let msg = &MessageVerification::pack_msg(&data, ts_ms, ttl_ms)?;
        let tx_id = msg.into();
        let addr = session_manager.authorizer()?.to_owned();
        let to_path = to_path.unwrap_or_default();
        let from_path = from_path.unwrap_or_default();

        let relay_verification = MessageVerification {
            session: session_manager.session()?,
            sig: session_manager.sign(msg)?,
            ttl_ms,
            ts_ms,
        };

        let origin_verification = match origin_verification_gen {
            OriginVerificationGen::Origin => relay_verification.clone(),
            OriginVerificationGen::Stick(ov) => ov,
        };

        Ok(Self {
            data,
            addr,
            tx_id,
            relay_verification,
            origin_verification,
            to_path,
            from_path,
            method,
        })
    }

    pub fn is_expired(&self) -> bool {
        let now = utils::get_epoch_ms();
        now > self.relay_verification.ts_ms + self.relay_verification.ttl_ms as u128
            && now > self.origin_verification.ts_ms + self.origin_verification.ttl_ms as u128
    }

    pub fn verify(&self) -> bool {
        if self.is_expired() {
            return false;
        }

        self.relay_verification.verify(&self.data) && self.origin_verification.verify(&self.data)
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
    where
        T: DeserializeOwned,
    {
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

impl<T> Encoder for MessageRelay<T>
where
    T: Serialize + DeserializeOwned,
{
    fn encode(&self) -> Result<Encoded> {
        self.gzip(9)?.encode()
    }
}

impl<T> Decoder for MessageRelay<T>
where
    T: Serialize + DeserializeOwned,
{
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Vec<u8> = encoded.decode()?;
        Self::from_auto(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ecc::SecretKey, transports::helper::TricklePayload};

    #[derive(Deserialize, Serialize, PartialEq, Debug)]
    struct TestData {
        a: String,
        b: i64,
        c: f64,
        d: bool,
    }

    fn new_test_message() -> MessageRelay<TestData> {
        let key = SecretKey::random();
        let session = SessionManager::new_with_seckey(&key).unwrap();
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        MessageRelay::new(
            test_data,
            &session,
            OriginVerificationGen::Origin,
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )
        .unwrap()
    }

    #[test]
    fn new_then_verify() {
        let payload = new_test_message();
        assert!(payload.verify());

        let key2 = SecretKey::random();
        let session2 = SessionManager::new_with_seckey(&key2).unwrap();
        let relaied_payload = MessageRelay::new(
            payload.data,
            &session2,
            OriginVerificationGen::Stick(payload.origin_verification),
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )
        .unwrap();
        assert!(relaied_payload.verify());
    }

    #[test]
    fn test_message_relay_gzip() {
        let payload = new_test_message();
        let gziped = payload.gzip(9).unwrap();
        let payload2: MessageRelay<TestData> = MessageRelay::from_gzipped(&gziped).unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_relay_from_auto() {
        let payload = new_test_message();
        let gziped_encoded_payload = payload.encode().unwrap();
        let payload2: MessageRelay<TestData> = gziped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let ungzip_encoded_payload = payload.to_json_vec().unwrap().encode().unwrap();
        let payload2: MessageRelay<TestData> = ungzip_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_relay_verify() {
        let key = SecretKey::random();
        let (auth, s_key) =
            SessionManager::gen_unsign_info(key.address(), None, Some(Signer::DEFAULT)).unwrap();
        let sig = key.sign(&auth.to_string().unwrap()).to_vec();
        let session = SessionManager::new(&sig, &auth, &s_key);

        let test_data = TricklePayload {
            sdp: "test_sdp".to_owned(),
            candidates: vec![],
        };
        let payload = MessageRelay::new(
            test_data,
            &session,
            OriginVerificationGen::Origin,
            None,
            None,
            None,
            MessageRelayMethod::SEND,
        )
        .unwrap();

        assert!(payload.verify(), "payload verify failed");
    }
}
