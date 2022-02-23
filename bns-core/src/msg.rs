use crate::ecc::{sign, verify, SecretKey};
use crate::encoder::Encoded;
use anyhow::anyhow;
use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::convert::TryFrom;
use web3::types::Address;

const DEFAULT_TTL_MS: usize = 60 * 1000;

#[derive(Deserialize, Serialize, Debug)]
pub struct SignedMsg<T> {
    pub data: T,
    pub ttl_ms: usize,
    pub ts_ms: u128,

    pub addr: Address,
    pub sig: Vec<u8>,
}

impl<T> SignedMsg<T>
where
    T: Serialize + DeserializeOwned,
{
    pub fn new(data: T, key: &SecretKey, ttl_ms: Option<usize>) -> Result<Self> {
        let ts_ms = get_epoch_ms();
        let ttl_ms = ttl_ms.unwrap_or(DEFAULT_TTL_MS);

        let msg = Self::pack_msg(&data, ts_ms, ttl_ms)?;
        let sig = sign(&msg, key).into();

        let addr = key.address().to_owned();

        Ok(Self {
            data,
            addr,
            sig,
            ttl_ms,
            ts_ms,
        })
    }

    pub fn is_expired(&self) -> bool {
        let now = get_epoch_ms();
        now < self.ts_ms + self.ttl_ms as u128
    }

    pub fn verify(&self) -> bool {
        if let Ok(msg) = Self::pack_msg(&self.data, self.ts_ms, self.ttl_ms) {
            verify(&msg, &self.addr, self.sig.clone())
        } else {
            false
        }
    }

    pub fn pack_msg(data: &T, ts_ms: u128, ttl_ms: usize) -> Result<String> {
        let mut msg = serde_json::to_string(data)?;
        msg.push_str(&format!("\n{}\n{}", ts_ms, ttl_ms));
        Ok(msg)
    }
}

impl<T> TryFrom<Encoded> for SignedMsg<T>
where
    T: Serialize + DeserializeOwned,
{
    type Error = anyhow::Error;
    fn try_from(s: Encoded) -> Result<Self> {
        let decoded: String = s.try_into()?;
        let data: SignedMsg<T> =
            serde_json::from_slice(decoded.as_bytes()).map_err(|e| anyhow!(e))?;
        Ok(data)
    }
}

impl<T> TryFrom<SignedMsg<T>> for Encoded
where
    T: Serialize + DeserializeOwned,
{
    type Error = anyhow::Error;
    fn try_from(s: SignedMsg<T>) -> Result<Self> {
        serde_json::to_string(&s)?.try_into()
    }
}

fn get_epoch_ms() -> u128 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
