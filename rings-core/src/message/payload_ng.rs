use crate::dht::Did;
use std::collections::VecDeque;
use crate::session::Session;
use crate::session::SessionManager;
use crate::ecc::{verify, HashStr};
use std::io::Write;
use flate2::write::{GzDecoder, GzEncoder};
use flate2::Compression;
use web3::types::Address;
use serde::de::DeserializeOwned;
use crate::err::{Error, Result};
use serde::Deserialize;
use serde::Serialize;
use crate::utils;
use super::encoder::{Decoder, Encoder};
use crate::message::Encoded;
use std::any::type_name;
use async_trait::async_trait;


const DEFAULT_TTL_MS: usize = 60 * 1000;

pub trait Message : Clone + Serialize + DeserializeOwned {
    type Output: Message;
    fn transform(&self) -> Option<Box<Self::Output>>;
}

#[async_trait]
pub trait MsgSender {
    async fn send<T>(msg: &RelayMessage<T>) -> Result<()>;
}

pub trait MessageRelay {
    fn sender(&self) -> Did;
    fn verify(&self) -> bool;
    fn flip(&self) -> Self;
    fn relay(&self, id: Did) -> Self;
    fn name(&self) -> String;
    fn next(&self) -> Option<Did>;
}

#[derive(Default)]
pub struct RelayMessageConfig {
    ttl_ms: Option<usize>,
    to_path: Option<VecDeque<Did>>,
    from_path: Option<VecDeque<Did>>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
#[serde(tag="method")]
pub enum RelayMessage<T> {
    SEND(Data<T>),
    REPORT(Data<T>)
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct Data<T> {
    pub data: T,
    pub tx_id: HashStr,
    pub ttl_ms: usize,
    pub ts_ms: u128,
    pub to_path: VecDeque<Did>,
    pub from_path: VecDeque<Did>,
    pub addr: Address,
    pub session: Session,
    pub sig: Vec<u8>,
}

impl<T> Data<T>
where
    T: Serialize + DeserializeOwned,
{
    // msg.flip()
    pub fn new(
        data: T,
        session_manager: &SessionManager,
        ttl_ms: Option<usize>,
        to_path: Option<VecDeque<Did>>,
        from_path: Option<VecDeque<Did>>,
    ) -> Result<Self> {
        let ts_ms = utils::get_epoch_ms();
        let ttl_ms = ttl_ms.unwrap_or(DEFAULT_TTL_MS);
        let session = session_manager.session()?;
        let data_str = Self::pack(&data, ts_ms, ttl_ms)?;
        let sig = session_manager.sign(&data_str)?;
        let tx_id = data_str.into();
        let addr = session_manager.authorizer()?.to_owned();
        let to_path = to_path.unwrap_or_default();
        let from_path = from_path.unwrap_or_default();

        Ok(
            Self {
                data,
                tx_id,
                ttl_ms,
                ts_ms,
                to_path,
                from_path,
                addr,
                session,
                sig
            }
        )
    }

    pub fn pack(data: &T, ts_ms: u128, ttl_ms: usize) -> Result<String> {
        let mut msg = serde_json::to_string(data).map_err(|_| Error::SerializeToString)?;
        msg.push_str(&format!("\n{}\n{}", ts_ms, ttl_ms));
        Ok(msg)
    }

}

impl <T> RelayMessage<T>
where
    T: Message,
{
    // always create SEND RelayMessage, you gan get REPORT message via
    // msg.flip()
    pub fn new(data: T, session_manager: &SessionManager) -> Result<Self> {
        Self::new_with_config(data, session_manager, RelayMessageConfig::default())
    }

    pub fn new_with_config(data: T, session_manager: &SessionManager, config: RelayMessageConfig) -> Result<Self> {
        let ttl_ms = config.ttl_ms;
        let to_path = config.to_path;
        let from_path = config.from_path;
        Ok(Self::SEND(
            Data::new(
                data,
                session_manager,
                ttl_ms,
                to_path,
                from_path
            )?
        ))
    }

    pub fn body(&self) -> &Data<T> {
        match self {
            Self::SEND(x) => x,
            Self::REPORT(x) => x
        }
    }

    pub fn is_expired(&self) -> bool {
        let now = utils::get_epoch_ms();
        now > self.body().ts_ms + self.body().ttl_ms as u128
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


/// https://www.cs.columbia.edu/~hgs/research/projects/MSRP/MSRP_lib_report.html
impl <T> MessageRelay for RelayMessage<T>
where
    T: Message,
{
    fn sender(&self) -> Did {
        self.body().addr.into()
    }

    fn verify(&self) -> bool {
        if !self.body().session.verify() {
            return false;
        }
        if self.is_expired() {
            return false;
        }
        if let (Ok(msg), Ok(addr)) = (
            Data::pack(&self.body().data, self.body().ts_ms, self.body().ttl_ms),
            self.body().session.address(),
        ) {
            verify(&msg, &addr, self.body().sig.clone())
        } else {
            false
        }

    }

    fn flip(&self) -> Self {
        match self {
            Self::SEND(x) => {
                let mut data: Data<T> = x.clone();
                (data.to_path, data.from_path) = (x.from_path.clone(), x.to_path.clone());
                Self::REPORT(data)
            },
            Self::REPORT(x) => {
                let mut data: Data<T> = x.clone();
                (data.to_path, data.from_path) = (x.from_path.clone(), x.to_path.clone());
                Self::SEND(data)
            }
        }
    }

    fn relay(&self, id: Did) -> Self {
        match self {
            Self::SEND(x) => {
                let mut data: Data<T> = x.clone();
                data.from_path.push_back(id);
                if let Some(a) = data.to_path.back() {
                    if *a == id {
                        data.to_path.pop_back();
                    }
                }
                Self::SEND(data)
            },
            Self::REPORT(x) => {
                let mut data: Data<T> = x.clone();
                if let Some(a) = data.from_path.back() {
                    if *a == id {
                        data.from_path.pop_back();
                    }
                }
                Self::REPORT(data)
            }
        }
    }

    fn name(&self) -> String {
        type_name::<T>().to_string()
    }

    fn next(&self) -> Option<Did> {
        match self {
            Self::SEND(x) => {
                if let Some(next) = x.to_path.get(0) {
                    if self.sender() != *next {
                        return Some(next.clone());
                    }
                }
                None
            },
            Self::REPORT(x) => {
                if let Some(next) = x.from_path.back() {
                    if self.sender() != *next {
                        return Some(next.clone());
                    }
                }
                None
            }
        }
    }
}


impl<T> Encoder for RelayMessage<T>
where
    T: Message,
{
    fn encode(&self) -> Result<Encoded> {
        self.gzip(9)?.encode()
    }
}

impl<T> Decoder for RelayMessage<T>
where
    T: Message,
{
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let v: Vec<u8> = encoded.decode()?;
        Self::from_auto(&v)
    }
}


#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::SecretKey;

    #[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
    struct TestData {
        a: String,
        b: i64,
        c: f64,
        d: bool,
    }

    #[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
    struct TestResp(u8);

    impl Message for () {
        type Output = ();
         fn transform(&self) -> Option<Box<Self::Output>> {
            Some(box ())
        }
    }

    impl Message for TestResp {
        type Output = ();
        fn transform(&self) -> Option<Box<Self::Output>> {
            Some(box ())
        }
    }

    impl Message for TestData {
        type Output = TestResp;
        fn transform(&self) -> Option<Box<Self::Output>> {
            Some(box TestResp(42))
        }
    }

    fn new_test_message() -> RelayMessage<TestData> {
        let key = SecretKey::random();
        let session = SessionManager::new_with_seckey(&key).unwrap();
        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };
        RelayMessage::new(
            test_data,
            &session,
        )
        .unwrap()
    }


    #[test]
    fn test_message_relay_gzip() {
        let payload = new_test_message();
        let gziped = payload.gzip(9).unwrap();
        let payload2: RelayMessage<TestData> = RelayMessage::from_gzipped(&gziped).unwrap();
        assert_eq!(payload, payload2);
    }

    #[test]
    fn test_message_relay_from_auto() {
        let payload = new_test_message();
        let gziped_encoded_payload = payload.encode().unwrap();
        let payload2: RelayMessage<TestData> = gziped_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);

        let ungzip_encoded_payload = payload.to_json_vec().unwrap().encode().unwrap();
        let payload2: RelayMessage<TestData> = ungzip_encoded_payload.decode().unwrap();
        assert_eq!(payload, payload2);
    }

}
