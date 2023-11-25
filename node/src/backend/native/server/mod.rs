mod tcp_proxy;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use rings_core::message::MessagePayload;
use rings_core::message::MessageVerificationExt;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::native::server::tcp_proxy::tcp_connect_with_timeout;
use crate::backend::native::server::tcp_proxy::Tunnel;
use crate::backend::native::MessageEndpoint;
use crate::backend::types::HttpRequest;
use crate::backend::types::HttpResponse;
use crate::backend::types::ServerMessage;
use crate::backend::types::TunnelId;
use crate::consts::TCP_SERVER_TIMEOUT;
use crate::error::Error;
use crate::error::Result;
use crate::processor::Processor;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// service name
    pub name: String,

    /// will register to dht storage if provided
    pub register_service: Option<String>,

    /// target address on server
    pub addr: SocketAddr,
}

pub struct Server {
    processor: Arc<Processor>,
    pub services: Vec<ServiceConfig>,
    pub tunnels: DashMap<TunnelId, Tunnel>,
}

impl Server {
    pub fn new(services: Vec<ServiceConfig>, processor: Arc<Processor>) -> Self {
        Self {
            processor,
            services,
            tunnels: DashMap::new(),
        }
    }
    fn service(&self, name: &str) -> Option<&ServiceConfig> {
        self.services
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(name))
    }
}

#[async_trait::async_trait]
impl MessageEndpoint<ServerMessage> for Server {
    async fn handle_message(&self, ctx: &MessagePayload, msg: &ServerMessage) -> Result<()> {
        let peer_did = ctx.transaction.signer();

        match msg {
            ServerMessage::TcpDial { tid, service } => {
                let service = self.service(service).ok_or(Error::InvalidService)?;
                match tcp_connect_with_timeout(service.addr, TCP_SERVER_TIMEOUT).await {
                    Err(e) => {
                        let msg = ServerMessage::TcpClose {
                            tid: *tid,
                            reason: e,
                        };
                        self.processor.send_backend_message(peer_did, msg).await?;
                        Err(Error::TunnelError(e))
                    }

                    Ok(local_stream) => {
                        let mut tunnel = Tunnel::new(*tid);
                        tunnel
                            .listen(local_stream, self.processor.clone(), peer_did)
                            .await;
                        self.tunnels.insert(*tid, tunnel);
                        Ok(())
                    }
                }
            }
            ServerMessage::TcpClose { tid, .. } => {
                self.tunnels.remove(tid);
                Ok(())
            }
            ServerMessage::TcpPackage { tid, body } => {
                self.tunnels
                    .get(tid)
                    .ok_or(Error::TunnelNotFound)?
                    .send(body.clone())
                    .await;
                Ok(())
            }
            ServerMessage::HttpRequest(req) => {
                let service = self.service(&req.service).ok_or(Error::InvalidService)?;
                let resp = handle_http_request(service.addr, req).await?;
                self.processor
                    .send_backend_message(peer_did, ServerMessage::HttpResponse(resp))
                    .await?;
                Ok(())
            }
            ServerMessage::HttpResponse(resp) => {
                tracing::info!("ServerMessage from {peer_did:?} HttpResponse: {resp:?}");
                Ok(())
            }
        }
    }
}

async fn handle_http_request(addr: SocketAddr, req: &HttpRequest) -> Result<HttpResponse> {
    let url = format!("{}/{}", addr, req.path.trim_start_matches('/'));
    let method = http::Method::from_str(req.method.as_str()).map_err(|_| Error::InvalidMethod)?;

    let headers_map: HashMap<String, String> = req.headers.iter().cloned().collect();
    let headers = (&headers_map).try_into().map_err(|e| {
        tracing::info!("invalid_headers: {}", e);
        Error::InvalidHeaders
    })?;

    let request = reqwest::Client::new()
        .request(method, url)
        .headers(headers)
        .timeout(Duration::from_secs(TCP_SERVER_TIMEOUT));

    let request = if let Some(body) = req.body.as_ref() {
        let body = body.to_vec();
        request.body(body)
    } else {
        request
    };

    let resp = request
        .send()
        .await
        .map_err(|e| Error::HttpRequestError(e.to_string()))?;

    let status = resp.status().as_u16();

    let headers = resp
        .headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_owned()))
        .collect();

    let body = resp
        .bytes()
        .await
        .map_err(|e| Error::HttpRequestError(e.to_string()))?;

    Ok(HttpResponse {
        status,
        headers,
        body: Some(body),
    })
}
