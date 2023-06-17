use std::cmp::Eq;
use std::cmp::PartialEq;
use std::error::Error;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::Read;
use std::io::Write;
use std::net::TcpStream;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::Weak;

use chrono::Utc;
use dashmap::DashMap;

use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::prelude::uuid;

/// TcpServicesError is an enumeration of possible errors that can occur in TCP services.
#[derive(Debug, thiserror::Error)]
pub enum TcpServicesError {
    /// This error occurs when parsing an address fails.
    #[error("Parse address error")]
    ParseError,
    /// This error wraps the standard IO error.
    #[error("IO Error: {0}")]
    IoError(std::io::Error),
    /// This error happens when a Mutex lock operation fails.
    #[error("Error when lock a Mutex {0}")]
    MutexLockError(String),
    /// This error happens when a RwLock lock operation fails.
    #[error("Error when lock a Rwlock {0}")]
    RwLockError(String),
    /// This error happens when the read size does not match the expected size.
    #[error("Size is not match")]
    ReadSizeNotMatch,
    #[error("Connection is dropped")]
    ConnectionDropped,
}

/// ConnectionInfo trait defines necessary methods for a connection.
/// This trait has a constant TIMEOUT which is defined at compile time.
pub trait ConnectionInfo<const TIMEOUT: u128>: Send + Sync + 'static {
    type Owner: Eq;
    type E: Error;
    /// Returns the owner of the connection.
    fn owner(&self) -> Result<Self::Owner, Self::E>;
    /// Returns true if the connection is timed out, else returns false.
    fn is_timeout(&self) -> Result<bool, Self::E>;
}

/// ConnectionIo trait defines the necessary I/O operations on a connection.
pub trait ConnectionIo: Send + Sync + 'static {
    type E: Error;
    /// Reads data from the connection.
    fn read(&self) -> Result<Box<Vec<u8>>, Self::E>;
    /// Writes data to the connection.
    fn write(&self, data: Vec<u8>) -> Result<bool, Self::E>;
}

/// ConnectionStatus trait defines the necessary status checker and operation.
pub trait ConnectionStatus: Send + Sync + 'static {
    type E: Error;
    /// Check the connection is disconnect
    fn is_disconnected(&self) -> Result<bool, Self::E>;
    /// Disconnect a connection
    fn disconnect(&self) -> Result<(), Self::E>;
    /// Check there is some msg in socket
    fn has_message(&self) -> Result<bool, Self::E>;
}

/// ConnectionPoolManager trait defines necessary methods to manage a connection pool.
pub trait ConnectionPoolManager<const SIZE: u16, const TIMEOUT: u128>:
    Send + Sync + 'static
{
    type Connection: Send + ConnectionInfo<TIMEOUT> + ConnectionIo + ConnectionStatus + 'static;
    type Error: Error + 'static;
    type Target: Send + 'static;
    type Owner: Send + Eq + Hash + 'static;
    type Key: Send + Eq + Hash + 'static;
    /// Establishes a new connection to the target and assigns it to an owner.
    fn new_connect(&self, target: Self::Target, owner: Self::Owner) -> Result<(), Self::Error>;
    /// Returns a connection given its key.
    fn get_connect(&self, k: Self::Key) -> Option<Self::Connection>;
    /// Returns true if the connection is connected, else returns false.
    fn is_connected(&self, conn: &Self::Connection) -> Result<bool, Self::Error>;
    /// Disconnects a connection, returns true if succeeded, else returns false.
    fn disconnect(&self, conn: &Self::Connection) -> Result<(), Self::Error>;
    // Remove disconnected connection from pool
    fn clear(&self);
    // Poll message, will returns a set of connections
    fn poll_message(&self) -> Vec<Self::Connection>;
}

/// TcpConnection struct represents a TCP connection with its meta data.
#[derive(Debug)]
pub struct TcpConnection {
    uuid: String,
    conn: Arc<Mutex<TcpStream>>,
    created_time: u128,
    last_active_time: Arc<RwLock<u128>>,
    owner: Did,
}

#[derive(Debug, Clone)]
pub struct TcpConnectionRef {
    uuid: String,
    conn: Weak<Mutex<TcpStream>>,
    created_time: u128,
    last_active_time: Weak<RwLock<u128>>,
    owner: Did,
}

impl TcpConnection {
    pub fn downgrade(&self) -> TcpConnectionRef {
        self.into()
    }
}

impl TcpConnectionRef {
    pub fn upgrade(&self) -> Result<TcpConnection, TcpServicesError> {
        self.deref().clone().try_into()
    }
}

impl From<&TcpConnection> for TcpConnectionRef {
    fn from(v: &TcpConnection) -> Self {
        Self {
            // clone
            uuid: v.uuid.clone(),
            // weakref
            conn: Arc::downgrade(&v.conn),
            // copy
            created_time: v.created_time,
            // weakref
            last_active_time: Arc::downgrade(&v.last_active_time),
            // clone
            owner: v.owner.clone(),
        }
    }
}

impl From<TcpConnectionRef> for Option<TcpConnection> {
    fn from(v: TcpConnectionRef) -> Self {
        let conn: Option<Arc<_>> = v.conn.upgrade();
        let last_active_time: Option<Arc<_>> = v.last_active_time.upgrade();
        match (conn, last_active_time) {
            (Some(c), Some(t)) => Some(TcpConnection {
                uuid: v.uuid.clone(),
                conn: c.clone(),
                created_time: v.created_time,
                last_active_time: t.clone(),
                owner: v.owner.clone(),
            }),
            (_, _) => None,
        }
    }
}

impl TryFrom<TcpConnectionRef> for TcpConnection {
    type Error = TcpServicesError;
    fn try_from(v: TcpConnectionRef) -> Result<Self, Self::Error> {
        let conn: Option<TcpConnection> = v.into();
        match conn {
            Some(c) => Ok(c),
            None => Err(Self::Error::ConnectionDropped),
        }
    }
}

impl PartialEq<Self> for TcpConnection {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl Eq for TcpConnection {}

/// This implementation ensures that TcpConnections are hashed based on their UUIDs.
impl Hash for TcpConnection {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl TcpConnection {
    /// Constructs a new TcpConnection to a specified address and assigns it to a specific owner.
    pub fn new(s: String, owner: Did) -> Result<Self, TcpServicesError> {
        let stream = TcpStream::connect(s).map_err(TcpServicesError::IoError)?;
        let now = Self::now();
        Ok(Self {
            uuid: Self::uuid(),
            conn: Arc::new(stream.into()),
            created_time: now,
            last_active_time: Arc::new(now.into()),
            owner: owner,
        })
    }

    /// Returns the current time in milliseconds.
    fn now() -> u128 {
        Utc::now().timestamp_millis() as u128
    }

    /// Generates a new UUID.
    fn uuid() -> String {
        uuid::Uuid::new_v4().to_simple().to_string()
    }

    /// Updates the last active time to the current time.
    fn visit(&self) -> Result<(), TcpServicesError> {
        let mut visited = self.last_active_time.write().map_err(|_| {
            TcpServicesError::RwLockError("Failed on write active_time".to_string())
        })?;
        *visited = Self::now();
        Ok(())
    }
}

impl<T: TryInto<TcpConnection, Error = TcpServicesError> + Sync + Send + Clone + 'static>
    ConnectionStatus for T
{
    type E = TcpServicesError;
    fn is_disconnected(&self) -> Result<bool, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let conn = ins
            .conn
            .lock()
            .map_err(|_| Self::E::MutexLockError("Failed on lock conn".to_string()))?;
        let mut buf = [0; 10];
        // try peek a litbit data there, if got error, the connection may closed
        if conn.peek(&mut buf).is_ok() {
            return Ok(false);
        } else {
            return Ok(true);
        }
    }

    fn disconnect(&self) -> Result<(), Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let conn = ins
            .conn
            .lock()
            .map_err(|_| Self::E::MutexLockError("Failed on lock conn".to_string()))?;
        conn.shutdown(std::net::Shutdown::Both)
            .map_err(Self::E::IoError)?;
        Ok(())
    }

    fn has_message(&self) -> Result<bool, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let conn = ins
            .conn
            .lock()
            .map_err(|_| Self::E::MutexLockError("Failed on lock conn".to_string()))?;
        let mut buf = [0; 10];
        let len = conn.peek(&mut buf).map_err(Self::E::IoError)?;
        Ok(len > 0)
    }
}

/// Implementation of ConnectionIo for TcpConnection.
impl<T: TryInto<TcpConnection, Error = TcpServicesError> + Sync + Send + Clone + 'static>
    ConnectionIo for T
{
    type E = TcpServicesError;

    fn read(&self) -> Result<Box<Vec<u8>>, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let mut conn = ins
            .conn
            .lock()
            .map_err(|_| Self::E::MutexLockError("Failed on lock conn".to_string()))?;
        let mut data: Vec<u8> = vec![];
        let size = (*conn).read_to_end(&mut data).map_err(Self::E::IoError)?;
        if !size == data.len() {
            return Err(Self::E::ReadSizeNotMatch);
        }
        ins.visit()?;
        Ok(Box::new(data))
    }

    fn write(&self, data: Vec<u8>) -> Result<bool, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let mut conn = ins
            .conn
            .lock()
            .map_err(|_| Self::E::MutexLockError("Failed on lock conn".to_string()))?;
        (*conn)
            .write_all(data.as_slice())
            .map_err(Self::E::IoError)?;
        ins.visit()?;
        Ok(true)
    }
}

/// Implementation of ConnectionInfo for TcpConnection.
impl<
        const TIMEOUT: u128,
        T: TryInto<TcpConnection, Error = TcpServicesError> + Sync + Send + Clone + 'static,
    > ConnectionInfo<TIMEOUT> for T
{
    type Owner = Did;
    type E = TcpServicesError;

    fn owner(&self) -> Result<Self::Owner, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        Ok(ins.owner)
    }

    fn is_timeout(&self) -> Result<bool, Self::E> {
        let ins: TcpConnection = self.deref().clone().try_into()?;
        let visited = ins
            .last_active_time
            .read()
            .map_err(|_| TcpServicesError::RwLockError("Failed on read active_time".to_string()))?;
        Ok(ins.created_time - *visited > TIMEOUT)
    }
}

/// ConnectionPool struct is a pool of TcpConnections.
pub struct ConnectionPool {
    pool: DashMap<String, TcpConnection>,
}

impl<const SIZE: u16, const TIMEOUT: u128> ConnectionPoolManager<SIZE, TIMEOUT> for ConnectionPool {
    type Connection = TcpConnectionRef;
    type Error = <TcpConnectionRef as ConnectionIo>::E;
    type Target = String;
    type Owner = Did;
    type Key = String;

    // Establishes a new connection and adds it to the pool.
    fn new_connect(&self, target: Self::Target, owner: Self::Owner) -> Result<(), Self::Error> {
        let conn = TcpConnection::new(target.clone(), owner.clone())?;
        self.pool.insert(conn.uuid.clone(), conn);
        Ok(())
    }

    // Returns a connection from the pool given its key.
    fn get_connect(&self, k: Self::Key) -> Option<Self::Connection> {
        self.pool.get(&k).map(|v| TcpConnectionRef::from(&*v))
    }

    // Returns true if the connection is connected.
    fn is_connected(&self, conn: &Self::Connection) -> Result<bool, Self::Error> {
        Ok(!(conn.is_disconnected()?))
    }

    // Disconnects a connection and removes it from the pool.
    fn disconnect(&self, conn: &Self::Connection) -> Result<(), Self::Error> {
        conn.disconnect()
    }

    // Remove a connection from pool
    fn clear(&self) {
        self.pool.retain(|_, c| {
            let c_ref = c.downgrade();
            match (
                c_ref.is_disconnected(),
                <TcpConnectionRef as ConnectionInfo<TIMEOUT>>::is_timeout(&c_ref),
            ) {
                (Ok(disconnect), Ok(timeout)) => disconnect || timeout,
                (_, _) => {
                    log::error!("Got error when clean connections");
                    false
                }
            }
        });
    }

    fn poll_message(&self) -> Vec<Self::Connection> {
        // rm disconnected connection first;
        <ConnectionPool as ConnectionPoolManager<SIZE, TIMEOUT>>::clear(self);
        let mut ret = vec![];
        for c in self.pool.iter() {
            let c_ref = c.downgrade();
            match c_ref.has_message() {
                Ok(true) => {
                    ret.push(c_ref.clone());
                }
                Ok(_) => (),
                Err(e) => {
                    log::error!("Got error when poll message! {:?}", e);
                }
            }
        }
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::rings_core::dht::Did;

    #[test]
    fn test_get_request() {
        let host = "www.bing.com:80".to_string();
        let request =
            "GET / HTTP/1.1\r\nHost: www.bing.com\r\nConnection: close\r\n\r\n".as_bytes();
        let did = Did::from(42u32);
        let conn = TcpConnection::new(host, did).unwrap();
        let conn_ref = conn.downgrade();
        conn_ref.write(request.to_vec()).unwrap();
        let resp = conn_ref.read().unwrap();
        assert!(resp.len() > 0)
    }
}
