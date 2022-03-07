use crate::ecc::{sign, verify, SecretKey};
use crate::message::Encoded;
use anyhow::anyhow;
use anyhow::Result;
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::convert::TryFrom;
use web3::types::Address;

const DEFAULT_TTL_MS: usize = 60 * 1000;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct MessagePayload<T> {
    pub data: T,
    pub ttl_ms: usize,
    pub ts_ms: u128,

    pub addr: Address,
    pub sig: Vec<u8>,
}

impl<T> MessagePayload<T>
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

    pub fn pubkey(&self) -> Result<PublicKey> {
        let msg = Self::pack_msg(&self.data, self.ts_ms, self.ttl_ms)?;
        recover(&msg, self.sig.clone())
    }

    pub fn pack_msg(data: &T, ts_ms: u128, ttl_ms: usize) -> Result<String> {
        let mut msg = serde_json::to_string(data)?;
        msg.push_str(&format!("\n{}\n{}", ts_ms, ttl_ms));
        Ok(msg)
    }
}

impl<T> TryFrom<Encoded> for MessagePayload<T>
where
    T: Serialize + DeserializeOwned,
{
    type Error = anyhow::Error;
    fn try_from(s: Encoded) -> Result<Self> {
        let decoded: String = s.try_into()?;
        let data: MessagePayload<T> =
            serde_json::from_slice(decoded.as_bytes()).map_err(|e| anyhow!(e))?;
        Ok(data)
    }
}

impl<T> TryFrom<MessagePayload<T>> for Encoded
where
    T: Serialize + DeserializeOwned,
{
    type Error = anyhow::Error;
    fn try_from(s: MessagePayload<T>) -> Result<Self> {
        serde_json::to_string(&s)?.try_into()
    }
}

fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Serialize)]
    struct TestData {
        a: String,
        b: i64,
        c: f64,
        d: bool,
    }

    #[test]
    fn new_then_verify() {
        let key =
            SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")
                .unwrap();

        let test_data = TestData {
            a: "hello".to_string(),
            b: 111,
            c: 2.33,
            d: true,
        };

        let payload = MessagePayload::new(test_data, &key, None).unwrap();

        assert!(payload.verify());
    }
}
