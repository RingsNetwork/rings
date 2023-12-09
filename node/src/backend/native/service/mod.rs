#![warn(missing_docs)]
//! This module provides the implementation of the Server of Rings Service.
//! Rings Service is a TCP/IP handler that can forward messages from a DHT (Distributed Hash Table) to
//! a local path. This module is essential for applications requiring decentralized network communication.
//!
//! # Service Config
//!
//! A Service Config is used to create a server instance. It contains the required parameters of
//! the services, describing how to forward messages to a local TCP socket. This configuration allows for
//! flexible and customized message routing based on specific application needs.
//!
//! # Service Provider
//!
//! A Rings Service Provider is a structure that serves Rings Service. Sometimes referred to as
//! "hidden-services," the Rings Service Provider exclusively handles the ServiceMessage type
//! of BackendMessage. This component is crucial for managing the flow of messages within decentralized networks.
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

use crate::backend::native::service::tcp_proxy::tcp_connect_with_timeout;
use crate::backend::native::service::tcp_proxy::Tunnel;
use crate::backend::native::MessageEndpoint;
use crate::backend::types::BackendMessage;
use crate::backend::types::HttpRequest;
use crate::backend::types::HttpResponse;
use crate::backend::types::ServiceMessage;
use crate::backend::types::TunnelId;
use crate::consts::TCP_SERVER_TIMEOUT;
use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::server::BackendMessageParams;
use crate::prelude::jsonrpc_core::Params;
use crate::provider::Provider;

/// Service Config for creating a Server instance
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// service name
    pub name: String,

    /// will register to dht storage if provided
    pub register_service: Option<String>,

    /// target address on server
    pub addr: SocketAddr,
}

/// Service Provider, which hold tunnel and a list of service
pub struct ServiceProvider {
    /// Service configs
    pub services: Vec<ServiceConfig>,
    /// Services tunnel, which is a HashMap of tunnel Id and Tunnel instance
    pub tunnels: DashMap<TunnelId, Tunnel>,
}

impl ServiceProvider {
    /// Create a new ServiceProvider with a config list
    pub fn new(services: Vec<ServiceConfig>) -> Self {
        Self {
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
impl MessageEndpoint<ServiceMessage> for ServiceProvider {
    async fn on_message(
        &self,
        provider: Arc<Provider>,
        ctx: &MessagePayload,
        msg: &ServiceMessage,
    ) -> Result<()> {
        let peer_did = ctx.transaction.signer();

        match msg {
            ServiceMessage::TcpDial { tid, service } => {
                let service = self.service(service).ok_or(Error::InvalidService)?;
                match tcp_connect_with_timeout(service.addr, TCP_SERVER_TIMEOUT).await {
                    Err(e) => {
                        let msg = ServiceMessage::TcpClose {
                            tid: *tid,
                            reason: e,
                        };
                        let backend_message: BackendMessage = msg.into();
                        let params: Params = BackendMessageParams {
                            did: peer_did,
                            data: backend_message,
                        }
                        .try_into()?;
                        provider
                            .request("sendBackendMessage".to_string(), params, None)
                            .await?;
                        Err(Error::TunnelError(e))
                    }

                    Ok(local_stream) => {
                        let mut tunnel = Tunnel::new(*tid);
                        tunnel
                            .listen(provider.clone(), local_stream, peer_did)
                            .await;
                        self.tunnels.insert(*tid, tunnel);
                        Ok(())
                    }
                }
            }
            ServiceMessage::TcpClose { tid, .. } => {
                self.tunnels.remove(tid);
                Ok(())
            }
            ServiceMessage::TcpPackage { tid, body } => {
                self.tunnels
                    .get(tid)
                    .ok_or(Error::TunnelNotFound)?
                    .send(body.clone())
                    .await;
                Ok(())
            }
            ServiceMessage::HttpRequest(req) => {
                let service = self.service(&req.service).ok_or(Error::InvalidService)?;
                let resp = handle_http_request(service.addr, req).await?;
                let msg: BackendMessage = ServiceMessage::HttpResponse(resp).into();
                let params: Params = BackendMessageParams {
                    did: peer_did,
                    data: msg,
                }
                .try_into()?;
                let resp = provider
                    .request("sendBackendMessage".to_string(), params, None)
                    .await?;
                tracing::info!("done calling provider {:?}", resp);
                Ok(())
            }
            ServiceMessage::HttpResponse(resp) => {
                tracing::info!("ServiceMessage from {peer_did:?} HttpResponse: {resp:?}");
                Ok(())
            }
        }
    }
}

async fn handle_http_request(addr: SocketAddr, req: &HttpRequest) -> Result<HttpResponse> {
    let url = format!("http://{}/{}", addr, req.path.trim_start_matches('/'));
    tracing::info!("Handle http request on url: {:?} start", url);
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
    tracing::info!("Handle http request done, responding");
    Ok(HttpResponse {
        status,
        headers,
        body: Some(body),
	rid: req.rid.clone()
    })
}
