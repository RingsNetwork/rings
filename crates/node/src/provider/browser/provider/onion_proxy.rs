//! The browser onion proxy: a target-agnostic handle that builds routes through the onion
//! directory and sends HTTPS requests over them.

use std::collections::BTreeSet;
use std::sync::Arc;

use rings_core::dht::Did;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use rings_rpc::jsonrpc::Client as RpcClient;
use rings_rpc::protos::rings_node::LookupOnionExitsRequest;
use rings_rpc::protos::rings_node::LookupOnlineNodesRequest;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use wasm_bindgen::JsError;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

use crate::error::Error;
use crate::error::Result as NodeResult;
use crate::extension::ext::Scope;
use crate::onion::circuit::route_first_hop;
use crate::onion::directory;
use crate::onion::directory::OnionDirectoryReader;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsClientRequest;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionRouteError;
use crate::online::OnlineNodeDescriptor;
use crate::processor::Processor;
use crate::provider::RemoteRpcEndpoint;

/// Browser-compatible onion proxy handle.
///
/// The proxy is target-agnostic: callers create it once with route-selection options, then send
/// absolute HTTPS URLs through it.
#[derive(Clone)]
#[wasm_export]
pub struct BrowserOnionProxy {
    pub(super) processor: Arc<Processor>,
    pub(super) scope: Scope,
    pub(super) config: OnionProxyConfig,
    pub(super) client: Arc<OnionHttpsClient>,
    pub(super) directory_endpoint: Option<RemoteRpcEndpoint>,
}

/// Typed response from a cancellable browser onion HTTPS request.
pub struct BrowserOnionProxyResponse {
    /// HTTP response returned by the selected onion exit.
    pub response: OnionHttpsResponse,
    /// Onion route used for the request.
    pub route: OnionProxyRoute,
}

/// Browser-facing projection of an internally selected onion route.
///
/// This is intentionally separate from the removed `buildOnionRoute` JSON-RPC request and
/// response. Browser proxy methods still return the route they actually used so their caller can
/// render and audit the result.
#[derive(serde::Serialize)]
struct BrowserOnionRouteInfo {
    /// Ordered DID hops ending with the exit.
    hops: Vec<String>,
    /// Canonical service selected by the route.
    service: String,
    /// Signed exit descriptor selected by the route.
    exit: OnionExitDescriptorInfo,
}

/// Project a selected route into the browser API without recreating the removed RPC method.
fn browser_onion_route_info(route: &crate::onion::OnionRoute) -> NodeResult<BrowserOnionRouteInfo> {
    Ok(BrowserOnionRouteInfo {
        hops: route.hops().iter().map(ToString::to_string).collect(),
        service: route.service().to_string(),
        exit: crate::rpc_dto::onion_exit_descriptor_info(route.exit().clone())?,
    })
}

#[derive(Clone)]
enum BrowserOnionDirectorySource {
    Local,
    Remote(RemoteRpcEndpoint),
}

struct BrowserOnionDirectoryReader {
    processor: Arc<Processor>,
    source: BrowserOnionDirectorySource,
}

impl BrowserOnionDirectoryReader {
    fn local(processor: Arc<Processor>) -> Self {
        Self {
            processor,
            source: BrowserOnionDirectorySource::Local,
        }
    }

    fn remote(processor: Arc<Processor>, endpoint: RemoteRpcEndpoint) -> Self {
        Self {
            processor,
            source: BrowserOnionDirectorySource::Remote(endpoint),
        }
    }

    fn direct_peer_dids(&self) -> BTreeSet<Did> {
        let local = self.processor.did();
        self.processor
            .swarm
            .peer_dids()
            .into_iter()
            .filter(|did| *did != local)
            .collect()
    }

    fn route_first_hop_is_direct(&self, route: &OnionProxyRoute) -> NodeResult<bool> {
        let first_hop = route_first_hop(&route.route)?;
        Ok(first_hop != self.processor.did() && self.direct_peer_dids().contains(&first_hop))
    }

    async fn read_online_nodes(&self) -> NodeResult<Vec<OnlineNodeDescriptor>> {
        match &self.source {
            BrowserOnionDirectorySource::Local => self.processor.lookup_online_nodes(false).await,
            BrowserOnionDirectorySource::Remote(endpoint) => {
                let response = authenticated_rpc_client(endpoint)?
                    .lookup_online_nodes(&LookupOnlineNodesRequest {
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::online_node_descriptors_from_infos(
                    response.nodes,
                    self.processor.swarm.network_id(),
                ))
            }
        }
    }

    async fn read_onion_exits(&self, service: &str) -> NodeResult<Vec<OnionExitDescriptor>> {
        match &self.source {
            BrowserOnionDirectorySource::Local => {
                self.processor.lookup_onion_exits(service, false).await
            }
            BrowserOnionDirectorySource::Remote(endpoint) => {
                let response = authenticated_rpc_client(endpoint)?
                    .lookup_onion_exits(&LookupOnionExitsRequest {
                        service: service.to_string(),
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::onion_exit_descriptors_from_infos(
                    response.exits,
                    self.processor.swarm.network_id(),
                ))
            }
        }
    }
}

/// Builds a directory RPC client under the shared credential transport policy.
fn authenticated_rpc_client(endpoint: &RemoteRpcEndpoint) -> NodeResult<RpcClient> {
    let client = RpcClient::new(endpoint.url.as_str())
        .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
    match &endpoint.api_token {
        Some(token) => client
            .with_bearer_token(token.to_string())
            .map_err(|error| Error::RemoteRpcError(error.to_string())),
        None => Ok(client),
    }
}

#[async_trait::async_trait(?Send)]
impl OnionDirectoryReader for BrowserOnionDirectoryReader {
    fn local_did(&self) -> Did {
        self.processor.did()
    }

    fn dht_protocol_mode(&self) -> DhtProtocolMode {
        self.processor.swarm.dht_protocol_mode()
    }

    async fn live_online_nodes(&self) -> NodeResult<Vec<OnlineNodeDescriptor>> {
        self.read_online_nodes().await
    }

    async fn live_onion_exits(&self, service: &str) -> NodeResult<Vec<OnionExitDescriptor>> {
        self.read_onion_exits(service).await
    }

    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)> {
        self.processor
            .peer_measurements()
            .await
            .into_iter()
            .map(|measurement| (measurement.did, measurement.quality))
            .collect()
    }

    fn onion_entry_guards(&self) -> &crate::onion::OnionEntryGuards {
        self.processor.onion_entry_guards()
    }
}

async fn build_browser_route_from_reader(
    reader: &BrowserOnionDirectoryReader,
    config: OnionProxyConfig,
    target: OnionProxyTarget,
) -> NodeResult<OnionProxyRoute> {
    let direct_peers = reader.direct_peer_dids();
    let route =
        directory::build_onion_proxy_route_with_first_hop(reader, config, target, move |did| {
            direct_peers.contains(&did)
        })
        .await?;
    if reader.route_first_hop_is_direct(&route)? {
        return Ok(route);
    }
    Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))
}

async fn build_browser_onion_proxy_route(
    processor: Arc<Processor>,
    config: OnionProxyConfig,
    target: OnionProxyTarget,
    directory_endpoint: Option<RemoteRpcEndpoint>,
) -> NodeResult<OnionProxyRoute> {
    if let Some(endpoint) = directory_endpoint {
        let remote_reader = BrowserOnionDirectoryReader::remote(processor.clone(), endpoint);
        match build_browser_route_from_reader(&remote_reader, config.clone(), target.clone()).await
        {
            Ok(route) => return Ok(route),
            Err(remote_error) => {
                let local_reader = BrowserOnionDirectoryReader::local(processor);
                return build_browser_route_from_reader(&local_reader, config, target)
                    .await
                    .map_err(|_| remote_error);
            }
        }
    }

    let local_reader = BrowserOnionDirectoryReader::local(processor);
    build_browser_route_from_reader(&local_reader, config, target).await
}

#[wasm_export]
impl BrowserOnionProxy {
    /// Return the exit service class this proxy selects.
    pub fn exit_service(&self) -> String {
        self.config.exit_service().to_string()
    }

    /// Return the desired hop count, including the exit. `0` means the node default.
    pub fn hop_count(&self) -> usize {
        self.config.hop_count
    }

    /// Return whether this proxy may use fewer hops when too few relays are live.
    pub fn allow_short_paths(&self) -> bool {
        self.config.allow_short_paths
    }

    /// Build an HTTPS-over-TCP onion proxy route for `target_authority` (`host:port`).
    pub fn route(&self, target_authority: String) -> js_sys::Promise {
        let proxy = self.clone();
        future_to_promise(async move {
            let route = proxy
                .route_http(&target_authority)
                .await
                .map_err(JsError::from)?;
            let response = browser_onion_route_info(&route.route).map_err(JsError::from)?;
            let value = js_value::serialize(&response).map_err(JsError::from)?;
            Ok(value)
        })
    }

    /// Send one HTTPS request through this onion proxy.
    ///
    /// `url` is an absolute `https://` URL. `request` is an object with optional `method`,
    /// `headers`, `body`, and `path` override fields. The returned Promise resolves to
    /// `{ status, headers, body }`.
    pub fn request(&self, url: String, request: JsValue) -> js_sys::Promise {
        let proxy = self.clone();
        future_to_promise(async move {
            let request = if request.is_null() || request.is_undefined() {
                OnionHttpsClientRequest {
                    method: "GET".to_string(),
                    path: None,
                    headers: Vec::new(),
                    body: Vec::new(),
                }
            } else {
                js_value::deserialize::<OnionHttpsClientRequest>(request).map_err(JsError::from)?
            };
            let response = proxy
                .request_http(url.as_str(), request)
                .await
                .map_err(JsError::from)?;
            let route_response =
                browser_onion_route_info(&response.route.route).map_err(JsError::from)?;
            let route_value = js_value::serialize(&route_response).map_err(JsError::from)?;
            let value = js_value::serialize(&response.response).map_err(JsError::from)?;
            js_sys::Reflect::set(&value, &JsValue::from_str("route"), &route_value)?;
            Ok(value)
        })
    }
}

impl BrowserOnionProxy {
    async fn build_route(&self, target: OnionProxyTarget) -> NodeResult<OnionProxyRoute> {
        build_browser_onion_proxy_route(
            self.processor.clone(),
            self.config.clone(),
            target,
            self.directory_endpoint.clone(),
        )
        .await
    }

    /// Build one typed HTTPS onion route without crossing a JavaScript promise boundary.
    pub async fn route_http(&self, target_authority: &str) -> NodeResult<OnionProxyRoute> {
        let target = OnionProxyTarget::parse_authority(target_authority)?;
        self.build_route(target).await
    }

    /// Send one typed HTTPS request through this proxy.
    ///
    /// Dropping the returned future cancels its pending circuit immediately. Browser frontends
    /// should use this method when their own request lifecycle can be cancelled.
    pub async fn request_http(
        &self,
        url: &str,
        request: OnionHttpsClientRequest,
    ) -> NodeResult<BrowserOnionProxyResponse> {
        let (target, call) = OnionHttpsCall::from_url(url, request)?;
        let route = self.build_route(target).await?;
        let response = self
            .client
            .request(self.scope.clone(), &route, call)
            .await?;
        Ok(BrowserOnionProxyResponse { response, route })
    }
}

#[cfg(test)]
mod tests;
