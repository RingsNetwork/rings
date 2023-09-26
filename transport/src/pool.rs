//! This module contains the [Pool] struct.

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::connection_ref::ConnectionRef;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;

/// [Pool] manages all the connections for each peer.
pub struct Pool<C> {
    connections: DashMap<String, Arc<C>>,
}

impl<C> Default for Pool<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> Pool<C> {
    /// Create a new [Pool] instance.
    pub fn new() -> Self {
        Self {
            connections: DashMap::new(),
        }
    }

    /// Get a reference of the connection by its id.
    pub fn connection(&self, cid: &str) -> Result<ConnectionRef<C>> {
        self.connections
            .get(cid)
            .map(|c| ConnectionRef::new(cid, c.value()))
            .ok_or(Error::ConnectionNotFound(cid.to_string()))
    }

    /// Get all the connections in the pool.
    pub fn connections(&self) -> Vec<(String, ConnectionRef<C>)> {
        self.connections
            .iter()
            .map(|kv| (kv.key().clone(), ConnectionRef::new(kv.key(), kv.value())))
            .collect()
    }

    /// Get all the connection ids in the pool.
    pub fn connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|kv| kv.key().clone()).collect()
    }
}

#[cfg(not(feature = "web-sys-webrtc"))]
impl<C, S> Pool<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S> + Send + Sync,
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// The `safely_insert` method is used to insert a connection into the pool.
    /// It ensures that the connection is not inserted twice in concurrent scenarios.
    ///
    /// The implementation of match statement refers to Entry::insert in dashmap.
    /// An extra check is added to see if the connection is already connected.
    /// See also: https://docs.rs/dashmap/latest/dashmap/mapref/entry/enum.Entry.html#method.insert
    pub fn safely_insert(&self, cid: &str, conn: C) -> Result<()> {
        let Some(entry) = self.connections.try_entry(cid.to_string()) else {
            return Err(Error::ConnectionAlreadyExists(cid.to_string()));
        };

        match entry {
            Entry::Occupied(mut entry) => {
                let existed_conn = entry.get();
                if matches!(
                    existed_conn.webrtc_connection_state(),
                    WebrtcConnectionState::New
                        | WebrtcConnectionState::Connecting
                        | WebrtcConnectionState::Connected
                ) {
                    return Err(Error::ConnectionAlreadyExists(cid.to_string()));
                }

                entry.insert(Arc::new(conn));
                entry.into_ref()
            }
            Entry::Vacant(entry) => entry.insert(Arc::new(conn)),
        };

        Ok(())
    }

    /// This method closes and releases the connection from pool.
    /// All references to this cid, created by `get_connection`, will be released.
    /// The [ConnectionInterface] methods of them will return [Error::ConnectionReleased].
    pub async fn safely_remove(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }
}

#[cfg(feature = "web-sys-webrtc")]
impl<C, S> Pool<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S>,
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// The `safely_insert` method is used to insert a connection into the pool.
    /// It ensures that the connection is not inserted twice in concurrent scenarios.
    ///
    /// The implementation of match statement refers to Entry::insert in dashmap.
    /// An extra check is added to see if the connection is already connected.
    /// See also: https://docs.rs/dashmap/latest/dashmap/mapref/entry/enum.Entry.html#method.insert
    pub fn safely_insert(&self, cid: &str, conn: C) -> Result<()> {
        let Some(entry) = self.connections.try_entry(cid.to_string()) else {
            return Err(Error::ConnectionAlreadyExists(cid.to_string()));
        };

        match entry {
            Entry::Occupied(mut entry) => {
                let existed_conn = entry.get();
                if matches!(
                    existed_conn.webrtc_connection_state(),
                    WebrtcConnectionState::New
                        | WebrtcConnectionState::Connecting
                        | WebrtcConnectionState::Connected
                ) {
                    return Err(Error::ConnectionAlreadyExists(cid.to_string()));
                }

                entry.insert(Arc::new(conn));
                entry.into_ref()
            }
            Entry::Vacant(entry) => entry.insert(Arc::new(conn)),
        };

        Ok(())
    }

    /// This method closes and releases the connection from pool.
    /// All references to this cid, created by `get_connection`, will be released.
    /// The [ConnectionInterface] methods of them will return [Error::ConnectionReleased].
    pub async fn safely_remove(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }
}
