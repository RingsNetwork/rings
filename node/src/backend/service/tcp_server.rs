#![warn(missing_docs)]

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::ErrorKind;
use tokio::net::TcpStream;
use tokio::net::ToSocketAddrs;
use tokio::time::timeout;

use crate::backend::types::BackendMessage;
use crate::backend::MessageEndpoint;
use crate::consts::TCP_SERVER_TIMEOUT;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::channels::Channel;
use crate::prelude::rings_core::dht::Did;
use crate::prelude::rings_core::prelude::dashmap::DashMap;
use crate::prelude::rings_core::prelude::uuid::Uuid;
use crate::prelude::rings_core::types::channel::Channel as ChannelTrait;
use crate::prelude::*;

/// HTTP Server Config, specific determine port.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TcpServiceConfig {
    /// name of hidden service
    pub name: String,

    /// will register to dht storage if provided
    pub register_service: Option<String>,

    /// address of hidden service
    pub addr: SocketAddr,
}

pub struct TcpServer {
    pub services: Vec<TcpServiceConfig>,
    pub connections: DashMap<Did, DashMap<Uuid, Connection>>,
}

pub struct Connection {
    sending_message_channel: Channel<MessagePayload<Message>>,
    receiving_message_channel: Channel<MessagePayload<Message>>,
    addr: SocketAddr,
    outbound: Option<TcpStream>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TcpInboundPackage {
    /// service name
    pub name: String,
    /// body
    pub body: Bytes,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TcpOutboundPackage {
    /// service name
    pub name: String,
    /// body
    pub body: Bytes,
}

impl TcpServer {
    /// Create a new instance of TcpServer
    pub fn new(services: Vec<TcpServiceConfig>) -> Self {
        Self {
            services,
            connections: DashMap::new(),
        }
    }

    pub async fn ensure_connection(
        self,
        did: Did,
        tx_id: Uuid,
        name: &str,
        addr: SocketAddr,
    ) -> Result<Arc<Connection>> {
        let conn = self
            .connections
            .entry((did, tx_id, name.clone()))
            .or_insert_with(|| Connection::new(addr));

        if conn.outbound.is_none() {
            let outbound = TcpStream::connect(&conn.addr).await?;

            conn.outbound = Some(outbound);
        }

        Ok(conn)
    }

    /// Listen and handle incoming messages
    pub async fn listen(&self) {}
}

#[async_trait::async_trait]
impl MessageEndpoint for TcpServer {
    async fn handle_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &BackendMessage,
    ) -> Result<Vec<MessageHandlerEvent>> {
        let inbound: TcpInbound =
            bincode::deserialize(&msg.data).map_err(|_| Error::DecodeError)?;

        let peer_did = ctx.origin().map_err(|_| Error::DecodeError)?;

        let service = self
            .services
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(inbound.name.as_str()))
            .ok_or(Error::InvalidService)?;

        let conn = self
            .ensure_connection(peer_did, service.name, service)
            .await?;

        conn.receive_message(inbound.body).await?;

        Ok(vec![])
    }
}

impl Connection {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            sending_message_channel: Channel::new(),
            receiving_message_channel: Channel::new(),
            addr,
            outbound: None,
        }
    }

    pub async fn receive_message(&self, msg: Bytes) -> Result<()> {
        self.receiving_message_channel.send(msg).await?;
        Ok(())
    }
}

pub async fn tcp_connect<T>(addr: T) -> Result<TcpStream>
where T: ToSocketAddrs {
    let fut = TcpStream::connect(addr);
    timeout(Duration::from_secs(TCP_SERVER_TIMEOUT), fut)
        .await
        .map_err(|_| Error::TcpConnectTimeout)?
        .map_err(|e| Error::TcpConnectError)
}
