mod callback;
pub mod connections;
pub mod core;
pub mod error;
pub mod ice_server;
mod notifier;

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;

use crate::core::transport::SharedConnection;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::error::Error;
use crate::error::Result;
use crate::ice_server::IceServer;

#[derive(Clone)]
pub struct Transport<C> {
    ice_servers: Vec<IceServer>,
    #[allow(dead_code)]
    external_address: Option<String>,
    connections: Arc<DashMap<String, C>>,
}

impl<C> Transport<C> {
    pub fn new(ice_servers: &str, external_address: Option<String>) -> Self {
        let ice_servers = IceServer::vec_from_str(ice_servers).unwrap();

        Self {
            ice_servers,
            external_address,
            connections: Arc::new(DashMap::new()),
        }
    }
}

impl<C> Transport<C>
where C: SharedConnection<Error = Error>
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

                entry.insert(conn);
                entry.into_ref()
            }
            Entry::Vacant(entry) => entry.insert(conn),
        };

        Ok(())
    }

    pub fn get_connection(&self, cid: &str) -> Result<C> {
        self.connections
            .get(cid)
            .map(|c| c.value().clone())
            .ok_or(Error::ConnectionNotFound(cid.to_string()))
    }

    pub fn get_connections(&self) -> Vec<(String, C)> {
        self.connections
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect()
    }

    pub fn get_connection_ids(&self) -> Vec<String> {
        self.connections.iter().map(|kv| kv.key().clone()).collect()
    }

    pub async fn send_message(&self, cid: &str, msg: TransportMessage) -> Result<()> {
        self.get_connection(cid)?.send_message(msg).await
    }

    pub async fn close_connection(&self, cid: &str) -> Result<()> {
        let Some((_, conn)) = self.connections.remove(cid) else {
            return Err(Error::ConnectionNotFound(cid.to_string()));
        };
        conn.close().await
    }
}
