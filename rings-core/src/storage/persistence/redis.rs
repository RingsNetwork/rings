use std::collections::HashMap;
use std::hash::Hash;
use std::string::ToString;
use std::thread::sleep;
use std::time::Duration;

use redis::cmd;
use redis::Client;
use redis::Commands;
use redis::Connection;
use redis::ConnectionAddr;
use redis::ConnectionInfo;
use redis::InfoDict;
use redis::RedisConnectionInfo;
use redis::Value;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{self};

use crate::err::Error;
use crate::err::Result;

/// Persistence Storage Operations
#[cfg(not(features = "wasm"))]
pub trait TPersistenceStorageOperation {
    /// Clear Storage.
    /// All `Entry` will be deleted.
    fn clear(&self) -> Result<()>;

    /// Get the current storage usage, if applicable.
    fn current_size(&self) -> Result<Option<u64>>;

    /// Get the maximum storage size, if applicable.
    fn max_size(&self) -> Result<Option<usize>>;
}

#[cfg(not(features = "wasm"))]
pub trait TPersistenceStorageRemove<K>: TPersistenceStorageOperation {
    /// Remove an `entry` by `key`.
    fn remove(&self, key: &K) -> Result<()>;

    /// Existence by `key`
    fn exists(&self, key: &K) -> Result<bool>;
}

#[cfg(not(features = "wasm"))]
pub trait TPersistenceStorageReadAndWrite<K, V>: TPersistenceStorageOperation {
    /// Get a cache entry by `key`.
    fn get(&self, key: &K) -> Result<V>;

    /// Put `entry` in the cache under `key`.
    fn put(&self, key: &K, entry: &V) -> Result<()>;
}

#[cfg(not(features = "wasm"))]
pub trait TRedisStorageBasic {
    fn get_client(&self) -> &Client;

    fn connect(&self) -> Result<Connection>;
}

#[cfg(not(features = "wasm"))]
pub struct RedisStorage {
    /// for display only: password (if any) will be masked
    client: Client,
}

impl RedisStorage {
    /// Create a new `RedisStrorage`.
    pub fn new(
        addr: &ConnectionAddr,
        redis_config: Option<RedisConnectionInfo>,
    ) -> Result<RedisStorage> {
        let client = {
            if let Some(config) = redis_config {
                Client::open(ConnectionInfo {
                    addr: addr.clone(),
                    redis: config,
                })?
            } else {
                Client::open(ConnectionInfo {
                    addr: addr.clone(),
                    redis: Default::default(),
                })?
            }
        };
        Ok(RedisStorage { client })
    }
}

impl TRedisStorageBasic for RedisStorage {
    /// Returns a redis Client
    fn get_client(&self) -> &Client {
        &self.client
    }

    /// Returns a connection with configured read and write timeouts.
    fn connect(&self) -> Result<Connection> {
        let con;
        let mut retries = 0;
        let millisecond = Duration::from_millis(1);
        loop {
            match self.client.get_connection() {
                Err(err) => {
                    if err.is_connection_refusal() {
                        sleep(millisecond);
                        retries += 1;
                        if retries > 100 {
                            panic!("Tried to connect too many times, last error: {}", err);
                        }
                    } else {
                        panic!("Could not connect: {}", err);
                    }
                }
                Ok(x) => {
                    con = x;
                    break;
                }
            }
        }
        Ok(con)
    }
}

impl TPersistenceStorageOperation for RedisStorage {
    /// Use FLUSHALL command clear all keys
    fn clear(&self) -> Result<()> {
        let mut con = self.connect()?;
        cmd("FLUSHALL").execute(&mut con);
        Ok(())
    }

    /// Returns the current size. This value is aquired via
    /// the Redis INFO command (used_memory).
    fn current_size(&self) -> Result<Option<u64>> {
        let mut con = self.connect()?;
        let v: InfoDict = cmd("INFO").query(&mut con)?;
        Ok(v.get("used_memory"))
    }

    /// Returns the maximum size. This value is read via
    /// the Redis CONFIG command (maxmemory). If the server has no
    /// configured limit, the result is None.
    fn max_size(&self) -> Result<Option<usize>> {
        let mut con = self.connect()?;
        let result: redis::RedisResult<HashMap<String, usize>> =
            cmd("CONFIG").arg("GET").arg("maxmemory").query(&mut con);
        match result {
            Ok(h) => Ok(h
                .get("maxmemory")
                .and_then(|&s| if s != 0 { Some(s) } else { None })),
            Err(_) => Ok(None),
        }
    }
}

impl<K, V, T> TPersistenceStorageReadAndWrite<K, V> for T
where
    K: Eq + Hash + ToString + std::marker::Sync,
    V: std::marker::Sync + DeserializeOwned + Serialize + redis::ToRedisArgs,
    T: TPersistenceStorageOperation + std::marker::Sync + TRedisStorageBasic,
{
    /// Open a connection and query for a key.
    fn get(&self, key: &K) -> Result<V> {
        let _key = key.to_string();
        let mut con = self.connect()?;
        match con.get(&_key)? {
            Value::Nil => Err(Error::RedisMiss),
            Value::Data(val) => serde_json::from_slice(&val).map_err(Error::Deserialize),
            _ => Err(Error::RedisInvalidKind),
        }
    }

    /// Open a connection and store a object in the .
    fn put(&self, key: &K, entry: &V) -> Result<()> {
        let _key = key.to_string();
        let mut con = self.connect()?;
        let value = serde_json::to_string(&entry).map_err(Error::Serialize)?;
        let _: () = redis::pipe()
            .atomic()
            .set(&_key, &value)
            .ignore()
            .expire(&_key, 60)
            .query(&mut con)?;
        Ok(())
    }
}

impl<K, T> TPersistenceStorageRemove<K> for T
where
    K: Eq + Hash + ToString + std::marker::Sync,
    T: TPersistenceStorageOperation + std::marker::Sync + TRedisStorageBasic,
{
    /// Delete key and storage
    fn remove(&self, key: &K) -> Result<()> {
        let mut con = self.connect()?;
        redis::cmd("DEL").arg(&key.to_string()).execute(&mut con);
        Ok(())
    }

    fn exists(&self, key: &K) -> Result<bool> {
        let _key = key.to_string();
        let mut con = self.connect()?;
        con.exists::<_, bool>(&_key).map_err(Error::RedisError)
    }
}

#[cfg(test)]
mod test {
    use std::env;
    use std::fs;
    use std::net::SocketAddr;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::process;

    use redis_derive::ToRedisArgs;
    use serde::Deserialize;
    use socket2::Domain;
    use socket2::Socket;
    use socket2::Type;

    use super::*;

    #[derive(PartialEq)]
    enum ServerType {
        Tcp { tls: bool },
        Unix,
    }

    impl ServerType {
        fn get_intended() -> ServerType {
            match env::var("REDISRS_SERVER_TYPE")
                .ok()
                .as_ref()
                .map(|x| &x[..])
            {
                Some("tcp") => ServerType::Tcp { tls: false },
                Some("tcp+tls") => ServerType::Tcp { tls: true },
                Some("unix") => ServerType::Unix,
                _ => {
                    // Default to Tcp without tls
                    //panic!("Unknown server type {:?}", val);
                    ServerType::Tcp { tls: false }
                }
            }
        }
    }

    struct RedisServer {
        pub process: process::Child,
        pub tempdir: Option<tempfile::TempDir>,
        addr: redis::ConnectionAddr,
    }

    impl RedisServer {
        pub fn new() -> RedisServer {
            let server_type = ServerType::get_intended();
            let addr = match server_type {
                ServerType::Tcp { tls } => {
                    let addr = &"127.0.0.1:0".parse::<SocketAddr>().unwrap().into();
                    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
                    socket.set_reuse_address(true).unwrap();
                    socket.bind(addr).unwrap();
                    socket.listen(1).unwrap();
                    let listener = TcpListener::from(socket);
                    let redis_port = listener.local_addr().unwrap().port();
                    if tls {
                        redis::ConnectionAddr::TcpTls {
                            host: "127.0.0.1".to_string(),
                            port: redis_port,
                            insecure: true,
                        }
                    } else {
                        redis::ConnectionAddr::Tcp("127.0.0.1".to_string(), redis_port)
                    }
                }
                ServerType::Unix => {
                    let (a, b) = rand::random::<(u64, u64)>();
                    let path = format!("/tmp/redis-rs-test-{}-{}.sock", a, b);
                    redis::ConnectionAddr::Unix(PathBuf::from(&path))
                }
            };
            RedisServer::new_with_addr(addr, |cmd| {
                cmd.spawn()
                    .unwrap_or_else(|err| panic!("Failed to run {:?}: {}", cmd, err))
            })
        }

        pub fn new_with_addr<F: FnOnce(&mut process::Command) -> process::Child>(
            addr: redis::ConnectionAddr,
            spawner: F,
        ) -> RedisServer {
            let mut redis_cmd = process::Command::new("redis-server");
            redis_cmd
                .stdout(process::Stdio::null())
                .stderr(process::Stdio::null());
            let tempdir = tempfile::Builder::new()
                .prefix("redis")
                .tempdir()
                .expect("failed to create tempdir");
            match addr {
                redis::ConnectionAddr::Tcp(ref bind, server_port) => {
                    redis_cmd
                        .arg("--port")
                        .arg(server_port.to_string())
                        .arg("--bind")
                        .arg(bind);

                    RedisServer {
                        process: spawner(&mut redis_cmd),
                        tempdir: None,
                        addr,
                    }
                }
                redis::ConnectionAddr::Unix(ref path) => {
                    redis_cmd
                        .arg("--port")
                        .arg("0")
                        .arg("--unixsocket")
                        .arg(&path);
                    RedisServer {
                        process: spawner(&mut redis_cmd),
                        tempdir: Some(tempdir),
                        addr,
                    }
                }
                _ => panic!("Not Support"),
            }
        }

        pub fn get_client_addr(&self) -> &redis::ConnectionAddr {
            &self.addr
        }

        pub fn get_tempdir(&self) -> &Option<tempfile::TempDir> {
            &self.tempdir
        }

        pub fn stop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
            if let redis::ConnectionAddr::Unix(ref path) = *self.get_client_addr() {
                fs::remove_file(&path).ok();
            }
        }
    }
    impl Drop for RedisServer {
        fn drop(&mut self) {
            self.stop()
        }
    }

    #[derive(Debug, Serialize, Deserialize, ToRedisArgs)]
    struct TestStorageStruct {
        content: String,
    }

    #[test]
    fn test_redis_operations() {
        let server = RedisServer::new();
        assert!(server.get_tempdir().is_none());
        let redis_storage = RedisStorage::new(server.get_client_addr(), None);
        assert!(redis_storage.is_ok());
        let redis_storage = redis_storage.unwrap();

        let before_used_memory = redis_storage.current_size().unwrap().unwrap();

        let key1 = "test1".to_owned();
        let data1 = TestStorageStruct {
            content: "test1".to_string(),
        };
        let _ = redis_storage.put(&key1, &data1).unwrap();
        let got_data1: TestStorageStruct = redis_storage.get(&key1).unwrap();

        assert!(
            got_data1.content.eq(data1.content.as_str()),
            "expect value1 is {}, got {}",
            data1.content,
            got_data1.content
        );

        assert!(redis_storage.exists(&key1).ok() == Some(true));

        let after_used_memory = redis_storage.current_size().unwrap().unwrap();
        assert!(after_used_memory > before_used_memory);

        let _ = redis_storage.remove(&key1).unwrap();

        assert!(
            redis_storage.exists(&key1).ok() == Some(false),
            "{}",
            redis_storage.exists(&key1).ok().unwrap()
        );
    }
}
