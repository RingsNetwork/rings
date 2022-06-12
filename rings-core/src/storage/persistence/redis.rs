use crate::err::{Error, Result};
use redis::{cmd, Client, Commands, Connection, InfoDict, Value};
use serde::de::DeserializeOwned;
use serde_json::{self};
use std::collections::HashMap;
use std::hash::Hash;
use std::string::ToString;
use url::Url;

/// A  that stores entries in a Redis.
#[cfg(not(features = "wasm"))]
#[derive(Clone)]
pub struct Redis {
    /// for display only: password (if any) will be masked
    pub display_url: String,
    client: Client,
}

/// Redis Taits about Storage
pub trait TRedisStorage<K, V> {
    type K;
    type V;

    /// Get a  entry by `key`.
    fn get(&self, key: &Self::K) -> Result<Self::V>;

    /// Put `entry` in the  under `key`.
    fn put(&self, key: &Self::K, entry: &Self::V) -> Result<()>;
}

impl Redis {
    /// Create a new `Redis`.
    pub fn new(url: &str) -> Result<Redis> {
        let mut parsed = Url::parse(url)?;
        // If the URL has a password set, mask it when displaying.
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("ring-redis-persistence"));
        }
        Ok(Redis {
            display_url: parsed.to_string(),
            client: Client::open(url)?,
        })
    }

    /// Returns a connection with configured read and write timeouts.
    fn connect(&self) -> Result<Connection> {
        Ok(self.client.get_connection()?)
    }

    /// Returns the maximum  size. This value is read via
    /// the Redis CONFIG command (maxmemory). If the server has no
    /// configured limit, the result is None.
    pub fn max_size(&self) -> Result<Option<u64>> {
        let mut con = self.connect()?;
        let result: redis::RedisResult<HashMap<String, usize>> =
            cmd("CONFIG").arg("GET").arg("maxmemory").query(&mut con);
        match result {
            Ok(h) => Ok(h
                .get("maxmemory")
                .and_then(|&s| if s != 0 { Some(s as u64) } else { None })),
            Err(_) => Ok(None),
        }
    }

    /// Returns the current  size. This value is aquired via
    /// the Redis INFO command (used_memory).
    pub fn current_size(&self) -> Result<Option<u64>> {
        let mut con = self.connect()?;
        let v: InfoDict = cmd("INFO").query(&mut con)?;
        Ok(v.get("used_memory"))
    }
}

impl<K, V> TRedisStorage<K, V> for Redis
where
    K: Clone + Eq + Hash + ToString + std::marker::Sync,
    V: Clone + redis::ToRedisArgs + std::marker::Sync + DeserializeOwned,
{
    type K = K;
    type V = V;

    /// Open a connection and query for a key.
    fn get(&self, key: &Self::K) -> Result<Self::V> {
        let _key = key.to_string();
        let mut con = self.connect()?;
        match con.get(&_key)? {
            Value::Nil => Err(Error::RedisMiss),
            Value::Data(val) => serde_json::from_slice(&val).map_err(Error::Deserialize),
            _ => Err(Error::RedisInvalidKind),
        }
    }

    /// Open a connection and store a object in the .
    fn put(&self, key: &Self::K, entry: &Self::V) -> Result<()> {
        let _key = key.to_string();
        let mut con = self.connect()?;
        let _: () = redis::pipe()
            .atomic()
            .set(&_key, &entry)
            .expire(&_key, 60)
            .query(&mut con)?;
        Ok(())
    }
}
