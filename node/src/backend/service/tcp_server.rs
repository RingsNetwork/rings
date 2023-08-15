#![warn(missing_docs)]

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::backend::service::proxy::tcp_connect_with_timeout;
use crate::backend::service::proxy::wrap_custom_message;
use crate::backend::service::proxy::Tunnel;
use crate::backend::service::proxy::TunnelId;
use crate::backend::service::proxy::TunnelMessage;
use crate::backend::types::BackendMessage;
use crate::backend::MessageEndpoint;
use crate::consts::TCP_SERVER_TIMEOUT;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::rings_core::prelude::dashmap::DashMap;
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
    pub tunnels: DashMap<TunnelId, Tunnel>,
    swarm: Option<&'static Swarm>,
}

impl TcpServer {
    /// Create a new instance of TcpServer
    pub fn new(services: Vec<TcpServiceConfig>) -> Self {
        Self {
            services,
            tunnels: DashMap::new(),
            swarm: None,
        }
    }

    pub fn bind(&mut self, swarm: &'static Swarm) {
        self.swarm = Some(swarm);
    }
}

#[async_trait::async_trait]
impl MessageEndpoint for TcpServer {
    async fn handle_message(
        &self,
        ctx: &MessagePayload<Message>,
        msg: &BackendMessage,
    ) -> Result<Vec<MessageHandlerEvent>> {
        let swarm = self.swarm.expect("swarm not bound");

        let peer_did = ctx.origin().map_err(|_| Error::DecodeError)?;
        let tunnel_msg: TunnelMessage =
            bincode::deserialize(&msg.data).map_err(|_| Error::DecodeError)?;

        match tunnel_msg {
            TunnelMessage::TcpDial { tid, service } => {
                let service = self
                    .services
                    .iter()
                    .find(|x| x.name.eq_ignore_ascii_case(&service))
                    .ok_or(Error::InvalidService)?;

                match tcp_connect_with_timeout(service.addr, TCP_SERVER_TIMEOUT).await {
                    Err(e) => {
                        let msg = TunnelMessage::TcpClose { tid, reason: e };
                        let custom_msg = wrap_custom_message(&msg);
                        swarm
                            .send_report_message(ctx, custom_msg)
                            .await
                            .map_err(Error::SendMessage)?;

                        Err(Error::TunnelError(e))?;
                    }

                    Ok(local_stream) => {
                        let mut tunnel = Tunnel::new(tid);
                        tunnel.listen(local_stream, swarm, peer_did).await;
                        self.tunnels.insert(tid, tunnel);
                    }
                }
            }
            TunnelMessage::TcpClose { tid, reason } => {
                self.tunnels.remove(&tid);
            }
            TunnelMessage::TcpPackage { tid, body } => {
                self.tunnels
                    .get(&tid)
                    .ok_or(Error::TunnelNotFound)?
                    .send(body)
                    .await;
            }
        }

        Ok(vec![])
    }
}
