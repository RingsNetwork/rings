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
use crate::onion::directory;
use crate::onion::directory::OnionDirectoryReader;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsClientRequest;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionLoopClass;
use crate::online::OnlineNodeDescriptor;
use crate::processor::Processor;
use crate::provider::BrowserDirectoryEndpoint;
use crate::remote_endpoint::attach_bearer_token;

/// Browser-compatible onion proxy handle.
///
/// The proxy is target-agnostic: callers create it once with route-selection options, then send
/// absolute HTTPS URLs through it.
#[derive(Clone)]
#[wasm_export]
pub struct BrowserOnionProxy {
    pub(super) processor: Arc<Processor>,
    pub(super) client: OnionHttpsClient,
    pub(super) directory_endpoint: Option<BrowserDirectoryEndpoint>,
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
    /// DIDs of the loop's positions `1 … H` in order, `g, r₀,₂, exit, r₁,₁, g`: the guard
    /// opens and closes the loop, so it is listed twice.
    hops: Vec<String>,
    /// Canonical service selected by the route.
    service: String,
    /// Signed exit descriptor selected by the route.
    exit: OnionExitDescriptorInfo,
}

/// Project a selected route into the browser API without recreating the removed RPC method.
fn browser_onion_route_info(route: &crate::onion::OnionRoute) -> NodeResult<BrowserOnionRouteInfo> {
    Ok(BrowserOnionRouteInfo {
        hops: route
            .hops()
            .positions()
            .map(|hop| hop.did.to_string())
            .collect(),
        service: route.service().to_string(),
        exit: crate::rpc_dto::onion_exit_descriptor_info(route.exit().clone())?,
    })
}

struct BrowserOnionDirectoryReader {
    processor: Arc<Processor>,
    endpoint: Option<BrowserDirectoryEndpoint>,
}

/// Builds a directory RPC client under the shared credential transport policy.
fn authenticated_rpc_client(endpoint: &BrowserDirectoryEndpoint) -> NodeResult<RpcClient> {
    RpcClient::new(endpoint.url.as_str())
        .map_err(|error| Error::RemoteRpcError(error.to_string()))
        .and_then(|client| attach_bearer_token(client, endpoint.api_token.as_deref()))
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
        match &self.endpoint {
            None => self.processor.lookup_online_nodes(false).await,
            Some(endpoint) => {
                let response = authenticated_rpc_client(endpoint)?
                    .lookup_online_nodes(&LookupOnlineNodesRequest {
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::online_node_descriptors_from_infos(
                    response.nodes,
                    self.processor.clock().now_ms(),
                    self.processor.swarm.network_id(),
                ))
            }
        }
    }

    async fn live_onion_exits(&self, service: &str) -> NodeResult<Vec<OnionExitDescriptor>> {
        match &self.endpoint {
            None => self.processor.lookup_onion_exits(service, false).await,
            Some(endpoint) => {
                let response = authenticated_rpc_client(endpoint)?
                    .lookup_onion_exits(&LookupOnionExitsRequest {
                        service: service.to_string(),
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::onion_exit_descriptors_from_infos(
                    response.exits,
                    self.processor.clock().now_ms(),
                    self.processor.swarm.network_id(),
                ))
            }
        }
    }

    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)> {
        self.processor
            .peer_measurements()
            .await
            .into_iter()
            .map(|measurement| (measurement.did, measurement.quality))
            .collect()
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

    fn onion_entry_guards(&self) -> &crate::onion::OnionEntryGuards {
        self.processor.onion_entry_guards()
    }
}

/// Build an HTTPS proxy route through `reader` whose first hop is a direct peer.
async fn build_browser_route_from_reader(
    reader: &BrowserOnionDirectoryReader,
    target: OnionProxyTarget,
) -> NodeResult<OnionProxyRoute> {
    directory::build_onion_proxy_route(reader, OnionProxyProtocol::HttpsProxy, target).await
}

async fn build_browser_onion_proxy_route(
    processor: Arc<Processor>,
    target: OnionProxyTarget,
    directory_endpoint: Option<BrowserDirectoryEndpoint>,
) -> NodeResult<OnionProxyRoute> {
    if let Some(endpoint) = directory_endpoint {
        let remote_reader = BrowserOnionDirectoryReader {
            processor: processor.clone(),
            endpoint: Some(endpoint),
        };
        match build_browser_route_from_reader(&remote_reader, target.clone()).await {
            Ok(route) => return Ok(route),
            Err(remote_error) => {
                let local_reader = BrowserOnionDirectoryReader {
                    processor,
                    endpoint: None,
                };
                return build_browser_route_from_reader(&local_reader, target)
                    .await
                    .map_err(|_| remote_error);
            }
        }
    }

    let local_reader = BrowserOnionDirectoryReader {
        processor,
        endpoint: None,
    };
    build_browser_route_from_reader(&local_reader, target).await
}

#[wasm_export]
impl BrowserOnionProxy {
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
    /// `headers`, `body`, `path` override, and `class` ([`OnionLoopClass`], defaulting to
    /// [`OnionLoopClass::DEFAULT`], `16 KiB`; every hop on the loop sees `b` and each cell costs
    /// `b / 16 KiB` admission units) fields. The returned Promise resolves to
    /// `{ status, headers, body, route }`.
    pub fn request(&self, url: String, request: JsValue) -> js_sys::Promise {
        let proxy = self.clone();
        future_to_promise(async move {
            let request = parse_browser_https_request(request).map_err(JsError::from)?;
            let response = proxy
                .request_http(url.as_str(), request)
                .await
                .map_err(JsError::from)?;
            let route_response =
                browser_onion_route_info(&response.route.route).map_err(JsError::from)?;
            let route_value = js_value::serialize(&route_response).map_err(JsError::from)?;
            let headers_value =
                js_value::serialize(&response.response.headers).map_err(JsError::from)?;
            let body_value = js_sys::Uint8Array::from(response.response.body.as_slice());
            let value = js_sys::Object::new();
            js_sys::Reflect::set(
                &value,
                &JsValue::from_str("status"),
                &JsValue::from_f64(f64::from(response.response.status)),
            )?;
            js_sys::Reflect::set(&value, &JsValue::from_str("headers"), &headers_value)?;
            js_sys::Reflect::set(&value, &JsValue::from_str("body"), &body_value)?;
            js_sys::Reflect::set(&value, &JsValue::from_str("route"), &route_value)?;
            Ok(value.into())
        })
    }
}

/// Parse a browser JS request argument into [`OnionHttpsClientRequest`], defaulting `null` and
/// `undefined` to a `GET` at [`OnionLoopClass::DEFAULT`].
pub(super) fn parse_browser_https_request(
    request: JsValue,
) -> Result<OnionHttpsClientRequest, rings_core::error::Error> {
    if request.is_null() || request.is_undefined() {
        Ok(OnionHttpsClientRequest {
            method: "GET".to_string(),
            ..OnionHttpsClientRequest::default()
        })
    } else {
        js_value::deserialize::<OnionHttpsClientRequest>(request)
    }
}

impl BrowserOnionProxy {
    async fn build_route(&self, target: OnionProxyTarget) -> NodeResult<OnionProxyRoute> {
        build_browser_onion_proxy_route(
            self.processor.clone(),
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

    /// Send one typed HTTPS request through this proxy at `request.class` (or
    /// [`OnionLoopClass::DEFAULT`], `16 KiB`, when `None`).
    ///
    /// Every hop on the loop sees the selected class `b` (#834 leakage table), and each cell of
    /// the loop costs `b / 16 KiB` admission units ([`OnionLoopClass::units`]).
    /// Dropping the returned future ends its session immediately. Browser frontends
    /// should use this method when their own request lifecycle can be cancelled.
    pub async fn request_http(
        &self,
        url: &str,
        request: OnionHttpsClientRequest,
    ) -> NodeResult<BrowserOnionProxyResponse> {
        let (target, call) = OnionHttpsCall::from_url(url, request)?;
        let route = self.build_route(target).await?;
        let response = self.client.request(&route, call).await?;
        Ok(BrowserOnionProxyResponse { response, route })
    }

    /// Send one typed HTTPS request through this proxy in a session of loop class `class` (`b`).
    ///
    /// Every hop on the loop sees `b` (#834 leakage table), and each cell of the loop costs
    /// `b / 16 KiB` admission units ([`OnionLoopClass::units`]).
    /// Dropping the returned future ends its session immediately.
    pub async fn request_http_with_class(
        &self,
        url: &str,
        request: OnionHttpsClientRequest,
        class: OnionLoopClass,
    ) -> NodeResult<BrowserOnionProxyResponse> {
        self.request_http(url, OnionHttpsClientRequest {
            class: Some(class),
            ..request
        })
        .await
    }
}

#[cfg(test)]
mod tests;
