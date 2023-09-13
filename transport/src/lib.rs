#![warn(missing_docs)]
#[doc = include_str!("../README.md")]
mod callback;
pub mod connection_ref;
pub mod connections;
pub mod core;
pub mod error;
pub mod ice_server;
mod notifier;

use std::sync::Arc;

use connection_ref::ConnectionRef;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::core::transport::ConnectionInterface;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceServer;

/// [Transport] is the main struct of this library.
/// It holds all the connections.
pub struct Transport<C> {
    ice_servers: Vec<IceServer>,
    #[allow(dead_code)]
    external_address: Option<String>,
    connections: DashMap<String, Arc<C>>,
}

impl<C> Transport<C> {
    /// Create a new [Transport] instance.
    /// Accept ice_servers for Connection
    pub fn new(ice_servers: &str, external_address: Option<String>) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            external_address,
            connections: DashMap::new(),
        }
    }
}

impl<C> Transport<C> {
    pub fn get_connection(&self, cid: &str) -> Result<ConnectionRef<C>> {
        self.connections
            .get(cid)
            .map(|c| ConnectionRef::new(cid, c.value()))
            .ok_or(Error::ConnectionNotFound(cid.to_string()))
    }

    pub fn get_connections(&self) -> Vec<(String, ConnectionRef<C>)> {
        self.connections
            .iter()
            .map(|kv| (kv.key().clone(), ConnectionRef::new(kv.key(), kv.value())))
            .collect()
    }

    pub fn get_connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|kv| kv.key().clone()).collect()
    }
}

#[cfg(not(feature = "web-sys-webrtc"))]
impl<C, S> Transport<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S> + Send + Sync,
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Safely insert
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

    pub async fn close_connection(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }
}

#[cfg(feature = "web-sys-webrtc")]
impl<C, S> Transport<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S>,
    S: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    /// Safely insert
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

    pub async fn close_connection(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }
}
