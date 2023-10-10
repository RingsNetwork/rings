#![warn(missing_docs)]

use std::str::FromStr;
use std::sync::Arc;

use rings_transport::core::callback::BoxedTransportCallback;
use rings_transport::core::transport::BoxedTransport;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::error::Error as TransportError;

use crate::dht::Did;
use crate::dht::PeerRing;
use crate::error::Error;
use crate::error::Result;
use crate::types::Connection;
use crate::types::ConnectionOwner;

pub struct RingsBackend {
    pub(crate) dht: Arc<PeerRing>,
    transport: BoxedTransport<ConnectionOwner, TransportError>,
}

impl RingsBackend {
    pub fn new(
        dht: Arc<PeerRing>,
        transport: BoxedTransport<ConnectionOwner, TransportError>,
    ) -> Self {
        Self { dht, transport }
    }

    /// Create new connection.
    pub async fn new_connection(
        &self,
        did: Did,
        transport_callback: Arc<BoxedTransportCallback>,
    ) -> Result<Connection> {
        let cid = did.to_string();
        self.transport
            .new_connection(&cid, transport_callback)
            .await
            .map_err(Error::Transport)?;
        self.transport.connection(&cid).map_err(|e| e.into())
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, did: Did) -> Result<()> {
        tracing::info!("[disconnect] removing from DHT {:?}", did);
        self.dht.remove(did)?;
        self.transport
            .close_connection(&did.to_string())
            .await
            .map_err(|e| e.into())
    }

    /// Get connection by did and check if it is connected.
    pub async fn get_and_check_connection(&self, did: Did) -> Option<Connection> {
        let cid = did.to_string();

        let Ok(c) = self.transport.connection(&cid) else {
            return None;
        };

        if c.is_connected().await {
            return Some(c);
        }

        tracing::debug!(
            "[get_and_check_connection] connection {did} is not connected, will be dropped"
        );

        if let Err(e) = self.disconnect(did).await {
            tracing::error!("Failed on close connection {did}: {e:?}");
        };

        None
    }

    /// Get connection by did.
    pub fn connection(&self, did: Did) -> Option<Connection> {
        self.transport.connection(&did.to_string()).ok()
    }

    /// Get all connections in transport.
    pub fn connections(&self) -> Vec<(Did, Connection)> {
        self.transport
            .connections()
            .into_iter()
            .filter_map(|(k, v)| Did::from_str(&k).ok().map(|did| (did, v)))
            .collect()
    }

    /// Get dids of all connections in transport.
    pub fn connection_ids(&self) -> Vec<Did> {
        self.transport
            .connection_ids()
            .into_iter()
            .filter_map(|k| Did::from_str(&k).ok())
            .collect()
    }
}
