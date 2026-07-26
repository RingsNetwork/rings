//! Browser Provider implementation
#![allow(non_snake_case, non_upper_case_globals, clippy::ptr_offset_with_cast)]
use std::collections::BTreeSet;
use std::convert::TryFrom;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use futures::future::Either;
use futures::FutureExt;
use js_sys;
use js_sys::Uint8Array;
use rings_core::dht::Did;
use rings_core::dht::EntryStorage;
use rings_core::ecc::PublicKey;
use rings_core::lifecycle::StopSource;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::prelude::entry;
use rings_core::prelude::entry::Entry;
use rings_core::storage::idb::IdbStorage;
use rings_core::utils::js_utils;
use rings_core::utils::js_value;
use rings_derive::wasm_export;
use rings_rpc::jsonrpc::Client as RpcClient;
use rings_rpc::protos::rings_node::*;
use wasm_bindgen;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures;
use wasm_bindgen_futures::future_to_promise;
use wasm_bindgen_futures::JsFuture;

use crate::error::Error;
use crate::error::Result as NodeResult;
use crate::measure::MeasureStorage;
use crate::measure::peer_quality_thresholds;
use crate::onion::circuit::encode_initial_forward;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::OnionCircuitCapabilities;
use crate::onion::circuit::OnionCircuitProtocol;
use crate::onion::circuit::OnionCircuitShell;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::directory;
use crate::onion::directory::OnionDirectoryReader;
use crate::onion::https::client_request_from_url as onion_https_client_request_from_url;
use crate::onion::https::encode_https_payload;
use crate::onion::https::BrowserOnionCircuitHandler;
use crate::onion::https::OnionHttpsClientRequest;
use crate::onion::https::OnionHttpsPayload;
use crate::onion::https::OnionHttpsRuntime;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionRouteError;
use crate::online::OnlineNodeDescriptor;
use crate::processor::Processor;
use crate::processor::ProcessorConfig;
use crate::provider::AsyncSigner;
use crate::provider::Provider;
use crate::provider::Signer;

/// AddressType enum contains `DEFAULT` and `ED25519`.
#[wasm_export]
pub enum AddressType {
    /// Default address type, hex string of sha1(pubkey)
    DEFAULT,
    /// Ed25519 style address type, hex string of pubkey
    Ed25519,
}

/// A wrapper of Arc Ref of Provider
#[derive(Clone)]
#[wasm_export]
pub struct ProviderRef {
    inner: Arc<Provider>,
}

impl ProviderRef {
    /// get wrapped arc, this is useful for wasm case
    pub fn inner(&self) -> Arc<Provider> {
        self.inner.clone()
    }
}

/// Browser listener lifecycle handle returned by [`Provider::listen`].
#[derive(Clone)]
#[wasm_export]
pub struct ProviderListener {
    stop: StopSource,
    task: js_sys::Promise,
}

#[wasm_export]
impl ProviderListener {
    /// Request cooperative shutdown for the listener task.
    pub fn stop(&self) {
        self.stop.request_stop();
    }

    /// Return whether shutdown was requested through this handle.
    pub fn is_stopped(&self) -> bool {
        self.stop.is_stop_requested()
    }

    /// Return the underlying listener task promise.
    ///
    /// It resolves only after [`ProviderListener::stop`] requests cooperative shutdown.
    pub fn task(&self) -> js_sys::Promise {
        self.task.clone()
    }
}

/// Browser-compatible onion proxy handle.
///
/// The proxy is target-agnostic: callers create it once with route-selection options, then send
/// absolute HTTPS URLs through it.
#[derive(Clone)]
#[wasm_export]
pub struct BrowserOnionProxy {
    processor: Arc<Processor>,
    config: OnionProxyConfig,
    runtime: Arc<OnionHttpsRuntime>,
    directory_endpoint: Option<String>,
}

#[derive(Clone)]
enum BrowserOnionDirectorySource {
    Local,
    Remote { endpoint_url: String },
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

    fn remote(processor: Arc<Processor>, endpoint_url: String) -> Self {
        Self {
            processor,
            source: BrowserOnionDirectorySource::Remote { endpoint_url },
        }
    }

    fn direct_peer_dids(&self) -> BTreeSet<Did> {
        let local = self.processor.did();
        self.processor
            .swarm
            .connected_peer_dids()
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
            BrowserOnionDirectorySource::Remote { endpoint_url } => {
                let response = RpcClient::new(endpoint_url.as_str())
                    .lookup_online_nodes(&LookupOnlineNodesRequest {
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::online_node_descriptors_from_infos(
                    response.nodes,
                ))
            }
        }
    }

    async fn read_onion_exits(&self, service: &str) -> NodeResult<Vec<OnionExitDescriptor>> {
        match &self.source {
            BrowserOnionDirectorySource::Local => {
                self.processor.lookup_onion_exits(service, false).await
            }
            BrowserOnionDirectorySource::Remote { endpoint_url } => {
                let response = RpcClient::new(endpoint_url.as_str())
                    .lookup_onion_exits(&LookupOnionExitsRequest {
                        service: service.to_string(),
                        include_expired: false,
                    })
                    .await
                    .map_err(|error| Error::RemoteRpcError(error.to_string()))?;
                Ok(crate::rpc_dto::onion_exit_descriptors_from_infos(
                    response.exits,
                ))
            }
        }
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
        let thresholds = peer_quality_thresholds();
        self.processor
            .peer_measurements()
            .await
            .into_iter()
            .map(|measurement| (measurement.did, measurement.evidence.classify(thresholds)))
            .collect()
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
    directory_endpoint: Option<String>,
) -> NodeResult<OnionProxyRoute> {
    if let Some(endpoint_url) = directory_endpoint {
        let remote_reader = BrowserOnionDirectoryReader::remote(processor.clone(), endpoint_url);
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
        let p = self.processor.clone();
        let config = self.config.clone();
        let directory_endpoint = self.directory_endpoint.clone();
        future_to_promise(async move {
            let target =
                OnionProxyTarget::parse_authority(&target_authority).map_err(JsError::from)?;
            let route = build_browser_onion_proxy_route(p, config, target, directory_endpoint)
                .await
                .map_err(JsError::from)?;
            let response =
                crate::rpc_dto::onion_route_response(route.route).map_err(JsError::from)?;
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
        let p = self.processor.clone();
        let config = self.config.clone();
        let runtime = self.runtime.clone();
        let directory_endpoint = self.directory_endpoint.clone();
        future_to_promise(async move {
            let request = if request.is_null() || request.is_undefined() {
                OnionHttpsClientRequest::default()
            } else {
                js_value::deserialize::<OnionHttpsClientRequest>(request).map_err(JsError::from)?
            };
            let (target, request) = onion_https_client_request_from_url(url.as_str(), request)
                .map_err(JsError::from)?;
            let proxy_route = build_browser_onion_proxy_route(
                p.clone(),
                config,
                target.clone(),
                directory_endpoint,
            )
            .await
            .map_err(JsError::from)?;
            let first_hop = route_first_hop(&proxy_route.route).map_err(JsError::from)?;
            let client_return = OnionClientReturn::new(p.session_sk().session_public_key());
            let (id, receiver) = runtime
                .begin_request(
                    first_hop,
                    proxy_route.route.exit().clone(),
                    client_return.return_id,
                )
                .map_err(JsError::from)?;
            let request_payload = match encode_https_payload(OnionHttpsPayload::Request(request)) {
                Ok(payload) => payload,
                Err(error) => {
                    runtime.cancel_request(id);
                    return Err(JsValue::from(JsError::from(error)));
                }
            };
            let (to, payload) = match encode_initial_forward(
                client_return,
                &proxy_route.route,
                id,
                request_payload,
            ) {
                Ok(encoded) => encoded,
                Err(error) => {
                    runtime.cancel_request(id);
                    return Err(JsValue::from(JsError::from(error)));
                }
            };
            let envelope =
                crate::extension::ext::Envelope::new(ONION_CIRCUIT_NAMESPACE.to_string(), payload);
            if let Err(error) = p.send_direct_envelope(to, &envelope).await {
                runtime.cancel_request(id);
                return Err(JsValue::from(JsError::from(error)));
            }
            let response = receiver.fuse();
            let timeout = js_utils::window_sleep(30_000).fuse();
            futures::pin_mut!(response, timeout);
            match futures::future::select(response, timeout).await {
                Either::Left((result, _)) => match result {
                    Ok(Ok(response)) => Ok(js_value::serialize(&response).map_err(JsError::from)?),
                    Ok(Err(error)) => Err(JsValue::from(JsError::from(error))),
                    Err(_) => Err(JsValue::from_str(
                        "onion HTTPS proxy response channel closed",
                    )),
                },
                Either::Right((_, _)) => {
                    runtime.cancel_request(id);
                    Err(JsValue::from_str("onion HTTPS proxy request timed out"))
                }
            }
        })
    }
}

fn wrapped_signer(signer: js_sys::Function) -> AsyncSigner {
    Box::new(
        move |data: String| -> Pin<Box<dyn Future<Output = Vec<u8>>>> {
            let signer = signer.clone();
            Box::pin(async move {
                let signer = signer.clone();
                let promise = match signer.call1(&JsValue::NULL, &JsValue::from_str(&data)) {
                    Ok(value) => js_sys::Promise::from(value),
                    Err(error) => {
                        tracing::error!("failed to call external JS signer: {error:?}");
                        return Vec::new();
                    }
                };
                let value = match JsFuture::from(promise).await {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::error!("external JS signer rejected: {error:?}");
                        return Vec::new();
                    }
                };
                let sig: js_sys::Uint8Array = Uint8Array::from(value);
                sig.to_vec()
            })
        },
    )
}

async fn open_browser_entry_storage(storage_name: &str) -> NodeResult<EntryStorage> {
    IdbStorage::new_with_cap_and_name(50000, storage_name)
        .await
        .map(|storage| Box::new(storage) as EntryStorage)
        .map_err(|source| Error::BrowserStorageOpen {
            name: storage_name.to_string(),
            source,
        })
}

async fn open_browser_entry_storage_or_memory(storage_name: &str) -> Option<EntryStorage> {
    match open_browser_entry_storage(storage_name).await {
        Ok(storage) => Some(storage),
        Err(error) => {
            tracing::warn!(
                storage_name = %storage_name,
                error = %error,
                "browser entry IndexedDB unavailable; falling back to in-memory entry storage"
            );
            None
        }
    }
}

async fn open_browser_measure_storage(storage_name: &str) -> Option<MeasureStorage> {
    match IdbStorage::new_with_cap_and_name(50000, storage_name).await {
        Ok(storage) => Some(Box::new(storage) as MeasureStorage),
        Err(source) => {
            tracing::warn!(
                storage_name = %storage_name,
                error = %source,
                "browser measurement IndexedDB unavailable; falling back to in-memory measurement storage"
            );
            None
        }
    }
}

impl Provider {
    /// Create a browser provider backed by IndexedDB storage and install its default backend.
    ///
    /// This is the Rust-side constructor for browser frontends. It keeps provider ownership as the
    /// lifecycle boundary while reusing the same storage, backend, and onion-protocol setup as the
    /// wasm-exported constructors.
    pub async fn new_browser_provider_with_storage(
        config: ProcessorConfig,
        storage_name: String,
    ) -> NodeResult<Self> {
        let onion_https_exit_policy = config.onion_https_exit_policy();
        let entry_storage = open_browser_entry_storage_or_memory(&storage_name).await;
        let measure_storage =
            open_browser_measure_storage(&format!("{storage_name}/measure")).await;

        let provider = Self::new_provider_with_storage_internal(
            config,
            entry_storage,
            measure_storage,
        )
        .await?;
        provider.set_backend()?;
        if let Some(policy) = onion_https_exit_policy {
            provider.install_onion_https_protocol(Some(policy))?;
        }
        Ok(provider)
    }
}

#[wasm_export]
impl Provider {
    /// make provider as an As arc ref
    pub fn as_ref(&self) -> ProviderRef {
        ProviderRef {
            inner: Arc::new(self.clone()),
        }
    }
}

#[wasm_export]
impl Provider {
    /// Create new instance of Provider, return Promise
    /// Ice_servers should obey forrmat: "[turn|strun]://<Address>:<Port>;..."
    /// Account is hex string
    /// Account should format as same as account_type declared
    /// Account_type is lowercase string, possible input are: `eip191`, `ed25519`, `bip137`, for more information,
    /// please check [rings_core::ecc]
    /// Signer should be `async function (proof: string): Promise<Unit8Array>`
    /// Signer should function as same as account_type declared, Eg: eip191 or secp256k1 or ed25519.
    #[wasm_bindgen(constructor)]
    pub fn new_instance(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: js_sys::Function,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let signer = wrapped_signer(signer);

            let entry_storage = open_browser_entry_storage_or_memory("rings-node").await;
            let measure_storage = open_browser_measure_storage("rings-node/measure").await;

            let provider = Provider::new_provider_internal(
                network_id,
                ice_servers,
                stabilize_interval,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                entry_storage,
                measure_storage,
            )
            .await?;

            provider.set_backend().map_err(JsError::from)?;

            Ok(JsValue::from(provider))
        })
    }

    /// Create a browser provider that advertises an HTTPS onion exit with explicit target policy.
    #[allow(clippy::too_many_arguments)]
    pub fn new_https_exit_instance(
        network_id: u32,
        ice_servers: String,
        stabilize_interval: u64,
        account: String,
        account_type: String,
        signer: js_sys::Function,
        allowed_targets: Vec<String>,
        denied_targets: Vec<String>,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let signer = wrapped_signer(signer);
            let policy = OnionExitPolicy::from_target_strings(allowed_targets, denied_targets)
                .map_err(JsError::from)?;
            policy.validate_targets().map_err(JsError::from)?;

            let entry_storage = open_browser_entry_storage_or_memory("rings-node").await;
            let measure_storage = open_browser_measure_storage("rings-node/measure").await;

            let config_policy = policy.clone();
            let provider = Provider::new_provider_internal_with_config(
                network_id,
                ice_servers,
                stabilize_interval,
                account,
                account_type,
                Signer::Async(Box::new(signer)),
                entry_storage,
                measure_storage,
                move |config| {
                    config
                        .enable_https_onion_exit()
                        .onion_exit_policy(config_policy)
                },
            )
            .await?;

            provider.set_backend().map_err(JsError::from)?;
            provider
                .install_onion_https_protocol(Some(policy))
                .map_err(JsError::from)?;

            Ok(JsValue::from(provider))
        })
    }

    /// Install the browser HTTPS onion-exit protocol handler with an explicit target policy.
    ///
    /// This updates the local exit handler used by incoming onion HTTPS requests. Discovery still
    /// comes from the provider's processor configuration, so nodes that should be routeable exits
    /// must also be constructed with HTTPS onion-exit advertisement enabled.
    pub fn install_onion_https_exit(
        &self,
        allowed_targets: Vec<String>,
        denied_targets: Vec<String>,
    ) -> Result<(), JsError> {
        let policy = OnionExitPolicy::from_target_strings(allowed_targets, denied_targets)
            .map_err(JsError::from)?;
        policy.validate_targets().map_err(JsError::from)?;
        self.install_onion_https_protocol(Some(policy))
            .map(|_| ())
            .map_err(JsError::from)
    }

    /// Create new provider instance with serialized config (yaml/json)
    pub fn new_provider_with_serialized_config(config: String) -> js_sys::Promise {
        future_to_promise(async move {
            let cfg: ProcessorConfig = serde_yaml::from_str(&config).map_err(JsError::from)?;
            JsFuture::from(Self::new_provider_with_config(cfg)).await
        })
    }

    /// Create a new provider instance.
    pub fn new_provider_with_config(config: ProcessorConfig) -> js_sys::Promise {
        Self::new_provider_with_storage(config, "rings-node".to_string())
    }

    /// get self web3 address
    #[wasm_bindgen(getter)]
    pub fn address(&self) -> String {
        self.processor.did().to_string()
    }

    ///  create new unsigned Provider
    pub fn new_provider_with_storage(
        config: ProcessorConfig,
        storage_name: String,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let provider = Self::new_browser_provider_with_storage(config, storage_name)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from(provider))
        })
    }

    /// Register a protocol handler: `provider.on(namespace, initialState, handler)`.
    ///
    /// `namespace` is the protocol namespace, `initialState` is the protocol's initial
    /// state, and `handler` is a pure transition `(ctx, event) -> { state, effects }`.
    /// The handler is bridged into the same pure model native uses; effects are run by
    /// the interpreter. The lower layer (JS vs native) is invisible — callers only ever
    /// see the provider.
    pub fn on(
        &self,
        namespace: String,
        initial_state: JsValue,
        handler: js_sys::Function,
    ) -> Result<(), JsError> {
        let protocol =
            crate::extension::protocols::js::JsProtocol::new(namespace, initial_state, handler);
        self.register_protocol(protocol, crate::extension::protocols::js::JsShell)
            .map_err(JsError::from)
    }

    /// Request local rpc interface
    pub fn request(&self, method: String, params: JsValue) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let params =
                js_value::json_value(params).map_err(|e| JsError::new(e.to_string().as_str()))?;
            let ret = ins
                .request_internal(method, params)
                .await
                .map_err(JsError::from)?;
            Ok(js_value::serialize(&ret).map_err(JsError::from)?)
        })
    }

    /// Start the long-running listener and return its lifecycle handle.
    pub fn listen(&self) -> ProviderListener {
        let p = self.processor.clone();
        let stop = StopSource::new();
        let token = stop.token();

        let task = future_to_promise(async move {
            p.listen_with(token).await;
            Ok(JsValue::null())
        });

        ProviderListener { stop, task }
    }

    /// connect peer with remote jsonrpc server url
    pub fn connect_peer_via_http(&self, remote_url: String) -> js_sys::Promise {
        log::debug!("remote_url: {remote_url}");
        let provider = self.clone();
        future_to_promise(async move {
            let params = serde_json::to_value(ConnectPeerViaHttpRequest {
                url: remote_url.clone(),
            })
            .map_err(JsError::from)?;
            let ret = provider
                .request_internal("connectPeerViaHttp".to_string(), params)
                .await
                .map_err(JsError::from)?;
            provider
                .set_onion_directory_endpoint(Some(remote_url))
                .map_err(JsError::from)?;
            Ok(js_value::serialize(&ret).map_err(JsError::from)?)
        })
    }

    /// connect peer with web3 address
    /// example:
    /// ```typescript
    /// const provider1 = new Provider()
    /// const provider2 = new Provider()
    /// const provider3 = new Provider()
    /// await create_connection(provider1, provider2);
    /// await create_connection(provider2, provider3);
    /// await provider1.connect_with_did(provider3.address())
    /// ```
    pub fn connect_with_address(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.connect_with_did(did).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// get info for self, will return build version and inspection of swarm
    pub fn get_node_info(&self) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let info = p.get_node_info().await.map_err(JsError::from)?;
            let v = js_value::serialize(&info).map_err(JsError::from)?;
            Ok(v)
        })
    }

    /// Get local measurement counters for a peer.
    pub fn get_peer_measurement(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let measurement = p.peer_measurement(did).await;
            let measurement = crate::rpc_dto::optional_peer_measurement_info(measurement)
                .map_err(JsError::from)?;
            let v = js_value::serialize(&measurement).map_err(JsError::from)?;
            Ok(v)
        })
    }

    /// disconnect a peer with web3 address
    pub fn disconnect(&self, address: String, addr_type: Option<AddressType>) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.disconnect(did).await.map_err(JsError::from)?;

            Ok(JsValue::from_str(did.to_string().as_str()))
        })
    }

    /// Send a namespaced message to a peer: `provider.send_message(did, namespace, payload)`.
    ///
    /// The payload reaches the peer's protocol registered under `namespace` (see
    /// [`Provider::on`]). This is the uniform upper-layer send, identical to native
    /// [`Provider::send`](crate::provider::Provider::send).
    pub fn send_message(
        &self,
        destination: String,
        namespace: String,
        payload: js_sys::Uint8Array,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(destination.as_str(), AddressType::DEFAULT)?;
            let envelope = crate::extension::ext::Envelope::new(namespace, payload.to_vec().into());
            let tx_id = p
                .send_envelope(did, &envelope)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from_str(tx_id.to_string().as_str()))
        })
    }

    /// Create a browser adapter for the standard HTTPS-over-TCP onion proxy service.
    ///
    /// The returned proxy is not bound to a URL; call [`BrowserOnionProxy::request`] with a full
    /// `https://` URL to send through the selected exit.
    pub fn onion_https_proxy(
        &self,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Result<BrowserOnionProxy, JsError> {
        let runtime = self
            .install_onion_https_protocol(None)
            .map_err(JsError::from)?;
        Ok(BrowserOnionProxy {
            processor: self.processor.clone(),
            config: OnionProxyConfig::https_proxy(hop_count, allow_short_paths),
            runtime,
            directory_endpoint: self.onion_directory_endpoint().map_err(JsError::from)?,
        })
    }

    /// Check local cache
    pub fn storage_check_cache(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            let v_node = p.storage_check_cache(did).await;
            if let Some(v) = v_node {
                let data = js_value::serialize(&v).map_err(JsError::from)?;
                Ok(data)
            } else {
                Ok(JsValue::null())
            }
        })
    }

    /// fetch storage with given did
    pub fn storage_fetch(
        &self,
        address: String,
        addr_type: Option<AddressType>,
    ) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let did = get_did(address.as_str(), addr_type.unwrap_or(AddressType::DEFAULT))?;
            p.storage_fetch(did).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// Store an entry on DHT storage
    pub fn storage_store(&self, data: String) -> js_sys::Promise {
        let p = self.processor.clone();
        future_to_promise(async move {
            let entry_info = entry::Entry::try_from(data).map_err(JsError::from)?;
            p.storage_store(entry_info).await.map_err(JsError::from)?;
            Ok(JsValue::null())
        })
    }

    /// lookup service did on DHT by its name
    /// - name: The name of service
    pub fn lookup_service(&self, name: String) -> js_sys::Promise {
        let p = self.processor.clone();

        future_to_promise(async move {
            let entry_key = Entry::gen_did(&name).map_err(JsError::from)?;

            tracing::debug!("browser lookup_service storage_fetch: {}", entry_key);
            p.storage_fetch(entry_key).await.map_err(JsError::from)?;
            tracing::debug!("browser lookup_service finish storage_fetch: {}", entry_key);
            js_utils::window_sleep(500).await?;
            let result = p.storage_check_cache(entry_key).await;

            if let Some(entry) = result {
                let dids = entry
                    .data
                    .iter()
                    .map(|v| v.decode())
                    .filter_map(|v| v.ok())
                    .map(|x: String| JsValue::from_str(x.as_str()))
                    .collect::<js_sys::Array>();
                Ok(JsValue::from(dids))
            } else {
                Ok(JsValue::from(js_sys::Array::new()))
            }
        })
    }
}

impl Provider {
    fn install_onion_https_protocol(
        &self,
        exit_policy: Option<OnionExitPolicy>,
    ) -> crate::error::Result<Arc<OnionHttpsRuntime>> {
        let allow_exit = exit_policy.is_some();
        let (runtime, registered) = {
            let mut slot = self
                .onion_https_runtime
                .lock()
                .map_err(|_| crate::error::Error::Lock)?;
            if let Some(runtime) = slot.as_ref() {
                (runtime.clone(), true)
            } else {
                let runtime = Arc::new(OnionHttpsRuntime::new());
                if self.extensions().contains(ONION_CIRCUIT_NAMESPACE) {
                    return Err(crate::error::Error::ExtensionError(format!(
                        "namespace {ONION_CIRCUIT_NAMESPACE:?} is already registered"
                    )));
                }
                let capabilities = OnionCircuitCapabilities::from_registration(
                    self.processor.advertise_onion_relay(),
                    allow_exit,
                );
                self.register_protocol(
                    OnionCircuitProtocol::new(capabilities),
                    self.onion_https_shell(runtime.clone()),
                )?;
                *slot = Some(runtime.clone());
                (runtime, false)
            }
        };

        if let Some(policy) = exit_policy {
            runtime.set_exit_policy(Some(policy));
            if registered {
                let capabilities = OnionCircuitCapabilities::from_registration(
                    self.processor.advertise_onion_relay(),
                    true,
                );
                self.extensions().replace(
                    OnionCircuitProtocol::new(capabilities),
                    self.onion_https_shell(runtime.clone()),
                )?;
            }
        }
        Ok(runtime)
    }

    fn onion_https_shell(
        &self,
        runtime: Arc<OnionHttpsRuntime>,
    ) -> OnionCircuitShell<BrowserOnionCircuitHandler> {
        OnionCircuitShell::new(
            self.processor.session_sk().clone(),
            BrowserOnionCircuitHandler::new(runtime, self.processor.session_sk().clone()),
        )
    }
}

fn get_did(address: &str, addr_type: AddressType) -> Result<Did, JsError> {
    let did = match addr_type {
        AddressType::DEFAULT => {
            Did::from_str(address).map_err(|_| JsError::new("invalid address"))?
        }
        AddressType::Ed25519 => PublicKey::try_from_b58t(address)
            .map_err(|_| JsError::new("invalid address"))?
            .address()
            .into(),
    };
    Ok(did)
}

/// Get address from hex pubkey
///  * pubkey: hex pubkey
#[wasm_export]
pub fn get_address_from_hex_pubkey(pubkey: String) -> Result<String, JsError> {
    Ok(Did::from(
        PublicKey::from_hex_string(pubkey.as_str())
            .map_err(JsError::from)?
            .address(),
    )
    .to_string())
}

/// Get address from other address
///   * address: source address
///   * addr_type: source address type
#[wasm_export]
pub fn get_address(address: &str, addr_type: AddressType) -> Result<String, JsError> {
    Ok(get_did(address, addr_type)?.to_string())
}
