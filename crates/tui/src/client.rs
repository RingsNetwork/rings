//! Asynchronous client adapter for querying and controlling a `rings-node` instance.

use std::fs;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::AUTHORIZATION;
use reqwest::StatusCode;
use rings_rpc::jsonrpc::Client as RpcClient;
use rings_rpc::protos::rings_node::AcceptAnswerRequest;
use rings_rpc::protos::rings_node::AnswerOfferRequest;
use rings_rpc::protos::rings_node::ConnectPeerViaHttpRequest;
use rings_rpc::protos::rings_node::ConnectWithDidRequest;
use rings_rpc::protos::rings_node::ConnectWithSeedRequest;
use rings_rpc::protos::rings_node::CreateOfferRequest;
use rings_rpc::protos::rings_node::DhtInfo;
use rings_rpc::protos::rings_node::DisconnectRequest;
use rings_rpc::protos::rings_node::FetchTopicMessagesRequest;
use rings_rpc::protos::rings_node::FingerTableRange;
use rings_rpc::protos::rings_node::ListPeerMeasurementsRequest;
use rings_rpc::protos::rings_node::ListPeersRequest;
use rings_rpc::protos::rings_node::LookupOnionExitsRequest;
use rings_rpc::protos::rings_node::LookupOnlineNodesRequest;
use rings_rpc::protos::rings_node::LookupServiceRequest;
use rings_rpc::protos::rings_node::NodeDidRequest;
use rings_rpc::protos::rings_node::NodeInfoRequest;
use rings_rpc::protos::rings_node::OnionExitDescriptorInfo;
use rings_rpc::protos::rings_node::OnionExitPolicyInfo;
use rings_rpc::protos::rings_node::OnlineNodeCapabilitiesInfo;
use rings_rpc::protos::rings_node::OnlineNodeDescriptorInfo;
use rings_rpc::protos::rings_node::OnlineNodeTypeInfo;
use rings_rpc::protos::rings_node::PeerCreditInfo;
use rings_rpc::protos::rings_node::PeerInfo;
use rings_rpc::protos::rings_node::PeerMeasurementCountersInfo;
use rings_rpc::protos::rings_node::PeerMeasurementInfo;
use rings_rpc::protos::rings_node::PeerMeasurementRequest;
use rings_rpc::protos::rings_node::PeerReliabilityInfo;
use rings_rpc::protos::rings_node::PublishMessageToTopicRequest;
use rings_rpc::protos::rings_node::RegisterServiceRequest;
use rings_rpc::protos::rings_node::SeedPeer;
use rings_rpc::protos::rings_node::SendBackendMessageRequest;
use rings_rpc::protos::rings_node::StorageInfo;
use rings_rpc::protos::rings_node::StorageItem;
use rings_rpc::protos::rings_node::StorageValue;
use rings_rpc::protos::rings_node::SwarmInfo;
use serde::Deserialize;

use crate::config::expand_home;
use crate::config::ResolvedTuiConfig;
use crate::error::Result;
use crate::error::TuiError;
use crate::types::decode_storage_element;
use crate::types::DhtKeyLookupReport;
use crate::types::DhtLookupSnapshotDto;
use crate::types::DhtLookupStatus;
use crate::types::DhtQueryTarget;
use crate::types::GatewayProbeState;
use crate::types::GatewayStatusDto;
use crate::types::HealthSnapshotDto;
use crate::types::LatencyBucketDto;
use crate::types::MailboxSnapshotDto;
use crate::types::MessageTotalsDto;
use crate::types::NodeSnapshot;
use crate::types::ObservabilityStatusDto;
use crate::types::OperatorSnapshotDto;
use crate::types::RecentMessageEventDto;
use crate::types::SessionKeySnapshotDto;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Maximum response body size (4 MiB) accepted from auxiliary HTTP status endpoints
/// (`/status?view=observability` and `/gateway/status`).
const MAX_STATUS_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

async fn read_bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<T> {
    if let Some(content_len) = response.content_length() {
        if content_len > max_bytes as u64 {
            return Err(TuiError::InvalidInput(format!(
                "status response Content-Length ({content_len} bytes) exceeds limit ({max_bytes} bytes)"
            )));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(TuiError::InvalidInput(format!(
                "status response body exceeded limit of {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<T>(&body).map_err(|err| {
        TuiError::InvalidInput(format!("failed to decode status JSON response: {err}"))
    })
}

/// Abstract interface for interacting with a Rings node, enabling both live HTTP/JSON-RPC
/// connections and deterministic in-memory test doubles.
#[async_trait]
pub trait NodeClient: Send + Sync {
    /// Collect a combined status, observability, gateway, and directory snapshot.
    async fn fetch_snapshot(&self, include_expired_directory: bool) -> Result<NodeSnapshot>;

    /// Connect to a remote peer via its HTTP RPC endpoint.
    async fn connect_peer_via_http(
        &self,
        url: &str,
        remote_api_token: Option<String>,
    ) -> Result<String>;

    /// Connect to a peer by DID over the Chord DHT.
    async fn connect_with_did(&self, did: &str) -> Result<()>;

    /// Connect to one or more seed peers from a seed file path or `did@url` specification.
    async fn connect_with_seed(&self, source: &str) -> Result<usize>;

    /// Disconnect from the specified peer DID.
    async fn disconnect(&self, did: &str) -> Result<()>;

    /// Create a manual WebRTC offer targeting `did`.
    async fn create_offer(&self, did: &str) -> Result<String>;

    /// Answer a peer's manual WebRTC offer.
    async fn answer_offer(&self, offer: &str) -> Result<String>;

    /// Accept a peer's manual WebRTC answer.
    async fn accept_answer(&self, answer: &str) -> Result<()>;

    /// Send a namespaced backend message (`data` is UTF-8 text that will be base64-encoded).
    async fn send_backend_message(
        &self,
        destination_did: &str,
        namespace: &str,
        data: &str,
    ) -> Result<()>;

    /// Publish a message to a DHT-backed PubSub topic.
    async fn publish_topic_message(&self, topic: &str, data: &str) -> Result<()>;

    /// Fetch messages from a DHT-backed PubSub topic starting at index `skip`.
    async fn fetch_topic_messages(&self, topic: &str, skip: i64) -> Result<Vec<String>>;

    /// Register the local node as a provider for service `name`.
    async fn register_service(&self, name: &str) -> Result<()>;

    /// Look up provider DIDs for service `name`.
    async fn lookup_service(&self, name: &str) -> Result<Vec<String>>;

    /// Query detailed local transport measurements for a single peer.
    async fn peer_measurement(&self, did: &str) -> Result<Option<PeerMeasurementInfo>>;

    /// Perform a Chord DHT key lookup by topic/service name or raw hex `Did` (`0x...`),
    /// combining live network fetch with local persistent and cache storage inspection.
    async fn dht_lookup_key(&self, key: &str) -> Result<DhtKeyLookupReport>;
}

/// Live HTTP + JSON-RPC client talking to a native `rings-node` internal listener.
pub struct HttpNodeClient {
    endpoint_url: String,
    auth_header: Option<HeaderValue>,
    rpc: RpcClient,
    http: reqwest::Client,
}

impl HttpNodeClient {
    /// Construct an [`HttpNodeClient`] from a resolved TUI configuration.
    pub fn from_config(config: &ResolvedTuiConfig) -> Result<Self> {
        Self::new(&config.endpoint_url, config.api_token.clone())
    }

    /// Construct an [`HttpNodeClient`] targeting `endpoint_url` with an optional Bearer token,
    /// validating both the endpoint URL and the HTTP `Authorization` header upfront.
    pub fn new(endpoint_url: &str, bearer_token: Option<String>) -> Result<Self> {
        let trimmed_url = endpoint_url.trim_end_matches('/').to_string();
        let auth_header = match &bearer_token {
            Some(token) => {
                if token.trim().is_empty() || token.chars().any(char::is_control) {
                    return Err(TuiError::InvalidInput(
                        "API token cannot be empty or contain control characters".to_string(),
                    ));
                }
                let hv = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|err| {
                    TuiError::InvalidInput(format!(
                        "API token is not a valid HTTP Authorization header value: {err}"
                    ))
                })?;
                Some(hv)
            }
            None => None,
        };

        let rpc_builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        let rpc = RpcClient::with_http_client_builder(&trimmed_url, rpc_builder)?;
        let rpc = match bearer_token {
            Some(token) => rpc.with_bearer_token(token)?,
            None => rpc,
        };

        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        Ok(Self {
            endpoint_url: trimmed_url,
            auth_header,
            rpc,
            http,
        })
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(val) = &self.auth_header {
            headers.insert(AUTHORIZATION, val.clone());
        }
        headers
    }

    async fn fetch_observability(&self) -> Result<ObservabilityStatusDto> {
        let url = format!("{}/status?view=observability", self.endpoint_url);
        let response = self
            .http
            .get(&url)
            .headers(self.auth_headers())
            .send()
            .await?;
        let response = response.error_for_status()?;
        read_bounded_json(response, MAX_STATUS_RESPONSE_BYTES).await
    }

    async fn fetch_gateway_status(&self) -> Result<Option<GatewayStatusDto>> {
        let url = format!("{}/gateway/status", self.endpoint_url);
        let response = self
            .http
            .get(&url)
            .headers(self.auth_headers())
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status()?;
        Ok(Some(
            read_bounded_json(response, MAX_STATUS_RESPONSE_BYTES).await?,
        ))
    }

    async fn fetch_all_peer_measurements(
        &self,
    ) -> Result<(Vec<PeerMeasurementInfo>, Option<String>)> {
        const PAGE_LIMIT: u32 = 64;
        const MAX_PAGES: usize = 16;
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        for page_idx in 0..MAX_PAGES {
            let req = ListPeerMeasurementsRequest {
                limit: Some(PAGE_LIMIT),
                cursor: cursor.clone(),
            };
            match self.rpc.list_peer_measurements(&req).await {
                Ok(resp) => {
                    let page_len = resp.measurements.len();
                    all.extend(resp.measurements);
                    match resp.next_cursor.filter(|c| !c.is_empty()) {
                        Some(next) if page_len > 0 => {
                            if page_idx + 1 == MAX_PAGES {
                                let total_len = all.len();
                                return Ok((
                                    all,
                                    Some(format!(
                                        "listPeerMeasurements: truncated after {total_len} records ({MAX_PAGES} pages)"
                                    )),
                                ));
                            }
                            cursor = Some(next);
                        }
                        _ => return Ok((all, None)),
                    }
                }
                Err(err) => {
                    if page_idx == 0 {
                        return Err(TuiError::from(err));
                    }
                    return Ok((
                        all,
                        Some(format!("listPeerMeasurements page {}: {err}", page_idx + 1)),
                    ));
                }
            }
        }
        Ok((all, None))
    }
}

/// Seed document structure compatible with JSON and YAML seed files.
#[derive(Deserialize)]
struct SeedDocument {
    peers: Vec<SeedPeer>,
}

/// Pure parser for a seed specification string (`<did>@<http-url>`, JSON document, or YAML document).
/// Does not perform any filesystem I/O.
pub fn parse_seed_str(raw: &str) -> Result<Vec<SeedPeer>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TuiError::InvalidInput(
            "seed source cannot be empty".to_string(),
        ));
    }

    if let Some((did, url)) = trimmed.split_once('@') {
        if !did.trim().is_empty() && (url.starts_with("http://") || url.starts_with("https://")) {
            return Ok(vec![SeedPeer {
                did: did.trim().to_string(),
                url: url.trim().to_string(),
                api_token: None,
            }]);
        }
    }

    if let Ok(doc) = serde_json::from_str::<SeedDocument>(trimmed) {
        return Ok(doc.peers);
    }
    if let Ok(doc) = serde_saphyr::from_str::<SeedDocument>(trimmed) {
        return Ok(doc.peers);
    }

    Err(TuiError::InvalidInput(
        "expected a seed file path, JSON/YAML with `peers: [...]`, or `<did>@<http-url>`"
            .to_string(),
    ))
}

/// Return a credential-safe display label for a seed source string, redacting inline JSON/YAML
/// documents so embedded `api_token` values are never logged in command history.
pub fn describe_seed_source(source: &str) -> String {
    let trimmed = source.trim();
    if trimmed.starts_with('{')
        || trimmed.starts_with("peers:")
        || trimmed.contains('\n')
        || trimmed.contains("api_token")
    {
        "<inline seed document>".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolve seed peers from a user-supplied source, which may either be an inline `<did>@<http-url>` /
/// JSON/YAML literal or a filesystem path to a seed file. Filesystem read errors are surfaced directly.
pub fn load_seed_peers(source: &str) -> Result<Vec<SeedPeer>> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(TuiError::InvalidInput(
            "seed source cannot be empty".to_string(),
        ));
    }

    // Try pure inline forms (`did@http...` or inline JSON/YAML) before touching the filesystem.
    if trimmed.contains('@')
        || trimmed.starts_with('{')
        || trimmed.starts_with("peers:")
        || trimmed.contains('\n')
        || trimmed.contains("api_token")
    {
        return parse_seed_str(trimmed);
    }

    let path = expand_home(trimmed.strip_prefix("file://").unwrap_or(trimmed));
    let contents = fs::read_to_string(&path).map_err(|err| {
        TuiError::InvalidInput(format!(
            "failed to read seed file {}: {err}",
            path.display()
        ))
    })?;
    parse_seed_str(&contents)
}

#[async_trait]
impl NodeClient for HttpNodeClient {
    async fn fetch_snapshot(&self, include_expired_directory: bool) -> Result<NodeSnapshot> {
        let started = Instant::now();
        let mut snapshot = NodeSnapshot {
            include_expired_directory,
            fetched_at_ms: now_epoch_ms(),
            ..NodeSnapshot::default()
        };

        let did_req = NodeDidRequest {};
        let info_req = NodeInfoRequest {};
        let peers_req = ListPeersRequest {};
        let online_req = LookupOnlineNodesRequest {
            include_expired: include_expired_directory,
        };
        let exits_req = LookupOnionExitsRequest {
            service: String::new(),
            include_expired: include_expired_directory,
        };

        let (did_res, info_res, peers_res, meas_res, obs_res, gw_res, online_res, exits_res) = tokio::join!(
            self.rpc.node_did(&did_req),
            self.rpc.node_info(&info_req),
            self.rpc.list_peers(&peers_req),
            self.fetch_all_peer_measurements(),
            self.fetch_observability(),
            self.fetch_gateway_status(),
            self.rpc.lookup_online_nodes(&online_req),
            self.rpc.lookup_onion_exits(&exits_req),
        );

        let mut authenticated_succeeded = false;
        let mut first_auth_err: Option<TuiError> = None;

        match did_res {
            Ok(resp) => {
                snapshot.did = Some(resp.did);
                snapshot.wire_version = resp.wire_version;
            }
            Err(err) => {
                snapshot.warnings.push(format!("nodeDid: {err}"));
            }
        }

        match info_res {
            Ok(info) => {
                authenticated_succeeded = true;
                snapshot.apply_node_info(info);
            }
            Err(err) => {
                snapshot.warnings.push(format!("nodeInfo: {err}"));
                first_auth_err.get_or_insert_with(|| TuiError::from(err));
            }
        }

        match peers_res {
            Ok(peers) => {
                authenticated_succeeded = true;
                snapshot.peers = peers.peers;
            }
            Err(err) => {
                snapshot.warnings.push(format!("listPeers: {err}"));
                first_auth_err.get_or_insert_with(|| TuiError::from(err));
            }
        }

        match meas_res {
            Ok((measurements, page_warn)) => {
                authenticated_succeeded = true;
                snapshot.measurements = measurements;
                if let Some(warn) = page_warn {
                    snapshot.warnings.push(warn);
                }
            }
            Err(err) => {
                snapshot
                    .warnings
                    .push(format!("listPeerMeasurements: {err}"));
            }
        }

        match obs_res {
            Ok(obs) => {
                authenticated_succeeded = true;
                if snapshot.version.is_none() {
                    snapshot.version = Some(obs.version);
                }
                snapshot.observability = Some(obs.observability);
            }
            Err(err) => {
                snapshot
                    .warnings
                    .push(format!("/status?view=observability: {err}"));
                first_auth_err.get_or_insert(err);
            }
        }

        match gw_res {
            Ok(Some(gw)) => {
                snapshot.gateway = GatewayProbeState::Available(gw);
            }
            Ok(None) => {
                snapshot.gateway = GatewayProbeState::NotMounted;
            }
            Err(err) => {
                let msg = err.to_string();
                snapshot.warnings.push(format!("/gateway/status: {msg}"));
                snapshot.gateway = GatewayProbeState::Failed(msg);
            }
        }

        match online_res {
            Ok(online) => {
                snapshot.online_nodes = online.nodes;
            }
            Err(err) => {
                let msg = err.to_string();
                snapshot.online_nodes_error = Some(msg.clone());
                snapshot.warnings.push(format!("lookupOnlineNodes: {msg}"));
            }
        }

        match exits_res {
            Ok(exits) => {
                snapshot.onion_exits = exits.exits;
            }
            Err(err) => {
                let msg = err.to_string();
                snapshot.onion_exits_error = Some(msg.clone());
                snapshot.warnings.push(format!("lookupOnionExits: {msg}"));
            }
        }

        if !authenticated_succeeded {
            return Err(first_auth_err.unwrap_or_else(|| {
                TuiError::InvalidInput(
                    "unable to authenticate or reach rings-node control endpoint".to_string(),
                )
            }));
        }

        snapshot.poll_latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(snapshot)
    }

    async fn connect_peer_via_http(
        &self,
        url: &str,
        remote_api_token: Option<String>,
    ) -> Result<String> {
        let resp = self
            .rpc
            .connect_peer_via_http(&ConnectPeerViaHttpRequest {
                url: url.trim().to_string(),
                api_token: remote_api_token.filter(|s| !s.trim().is_empty()),
            })
            .await?;
        Ok(resp.did)
    }

    async fn connect_with_did(&self, did: &str) -> Result<()> {
        self.rpc
            .connect_with_did(&ConnectWithDidRequest {
                did: did.trim().to_string(),
            })
            .await?;
        Ok(())
    }

    async fn connect_with_seed(&self, source: &str) -> Result<usize> {
        let peers = load_seed_peers(source)?;
        let count = peers.len();
        self.rpc
            .connect_with_seed(&ConnectWithSeedRequest { peers })
            .await?;
        Ok(count)
    }

    async fn disconnect(&self, did: &str) -> Result<()> {
        self.rpc
            .disconnect(&DisconnectRequest {
                did: did.trim().to_string(),
            })
            .await?;
        Ok(())
    }

    async fn create_offer(&self, did: &str) -> Result<String> {
        let resp = self
            .rpc
            .create_offer(&CreateOfferRequest {
                did: did.trim().to_string(),
            })
            .await?;
        Ok(resp.offer)
    }

    async fn answer_offer(&self, offer: &str) -> Result<String> {
        let resp = self
            .rpc
            .answer_offer(&AnswerOfferRequest {
                offer: offer.trim().to_string(),
            })
            .await?;
        Ok(resp.answer)
    }

    async fn accept_answer(&self, answer: &str) -> Result<()> {
        self.rpc
            .accept_answer(&AcceptAnswerRequest {
                answer: answer.trim().to_string(),
            })
            .await?;
        Ok(())
    }

    async fn send_backend_message(
        &self,
        destination_did: &str,
        namespace: &str,
        data: &str,
    ) -> Result<()> {
        self.rpc
            .send_backend_message(&SendBackendMessageRequest {
                destination_did: destination_did.trim().to_string(),
                namespace: namespace.trim().to_string(),
                data: base64::encode(data.as_bytes()),
            })
            .await?;
        Ok(())
    }

    async fn publish_topic_message(&self, topic: &str, data: &str) -> Result<()> {
        self.rpc
            .publish_message_to_topic(&PublishMessageToTopicRequest {
                topic: topic.trim().to_string(),
                data: data.to_string(),
            })
            .await?;
        Ok(())
    }

    async fn fetch_topic_messages(&self, topic: &str, skip: i64) -> Result<Vec<String>> {
        let resp = self
            .rpc
            .fetch_topic_messages(&FetchTopicMessagesRequest {
                topic: topic.trim().to_string(),
                skip,
            })
            .await?;
        Ok(resp.data)
    }

    async fn register_service(&self, name: &str) -> Result<()> {
        self.rpc
            .register_service(&RegisterServiceRequest {
                name: name.trim().to_string(),
            })
            .await?;
        Ok(())
    }

    async fn lookup_service(&self, name: &str) -> Result<Vec<String>> {
        let resp = self
            .rpc
            .lookup_service(&LookupServiceRequest {
                name: name.trim().to_string(),
            })
            .await?;
        Ok(resp.dids)
    }

    async fn peer_measurement(&self, did: &str) -> Result<Option<PeerMeasurementInfo>> {
        let resp = self
            .rpc
            .peer_measurement(&PeerMeasurementRequest {
                did: did.trim().to_string(),
            })
            .await?;
        Ok(resp.measurement)
    }

    async fn dht_lookup_key(&self, key: &str) -> Result<DhtKeyLookupReport> {
        let started = Instant::now();
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err(TuiError::InvalidInput(
                "DHT lookup key cannot be empty".to_string(),
            ));
        }
        let target = DhtQueryTarget::parse(trimmed);

        let query_mode = target.mode_label().to_string();
        let target_did = target.target_entry_did().to_string();
        let mut sources = Vec::new();
        let mut elements = Vec::new();
        let mut record_kind = "None".to_string();
        let mut warnings = Vec::new();

        match &target {
            DhtQueryTarget::TopicKey {
                topic,
                derived_entry_did,
            } => {
                let fetch_req = FetchTopicMessagesRequest {
                    topic: topic.clone(),
                    skip: 0,
                };
                let info_req = NodeInfoRequest {};
                let (fetch_res, info_res) = tokio::join!(
                    self.rpc.fetch_topic_messages(&fetch_req),
                    self.rpc.node_info(&info_req),
                );

                if fetch_res.is_err() && info_res.is_err() {
                    if let Err(err) = fetch_res {
                        return Err(TuiError::from(err));
                    }
                }

                match fetch_res {
                    Ok(fetched) => {
                        if !fetched.data.is_empty() {
                            sources.push("DHT Topic RPC (Network/Cache)".to_string());
                            record_kind = "Data/Append".to_string();
                            elements.extend(fetched.data);
                        }
                    }
                    Err(err) => {
                        warnings.push(format!("fetchTopicMessages: {err}"));
                    }
                }

                match info_res {
                    Ok(info) => {
                        collect_matching_storage_items(
                            info.swarm.as_ref(),
                            Some(topic.as_str()),
                            derived_entry_did.as_str(),
                            &mut sources,
                            &mut record_kind,
                            &mut elements,
                        );
                    }
                    Err(err) => {
                        warnings.push(format!("nodeInfo: {err}"));
                    }
                }
            }
            DhtQueryTarget::RawEntryDid { entry_did } => {
                // Do not call `fetchTopicMessages` on a raw `0x...` Entry DID because the server's
                // `fetch_text_elements` hashes its string parameter via SHA-1 (`Entry::gen_did`),
                // which would double-hash the raw DID and query the wrong VNode address.
                let info_req = NodeInfoRequest {};
                let info = self.rpc.node_info(&info_req).await?;
                collect_matching_storage_items(
                    info.swarm.as_ref(),
                    None,
                    entry_did.as_str(),
                    &mut sources,
                    &mut record_kind,
                    &mut elements,
                );
            }
        }

        let status = if !warnings.is_empty() {
            DhtLookupStatus::PartialError
        } else if !sources.is_empty() {
            DhtLookupStatus::Found
        } else {
            DhtLookupStatus::Absent
        };

        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(DhtKeyLookupReport {
            query_key: trimmed.to_string(),
            query_mode,
            target_did,
            status,
            sources,
            record_kind,
            elements,
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
            duration_ms,
        })
    }
}

fn collect_matching_storage_items(
    swarm_opt: Option<&SwarmInfo>,
    exact_topic_key: Option<&str>,
    target_entry_did: &str,
    sources: &mut Vec<String>,
    record_kind: &mut String,
    elements: &mut Vec<String>,
) {
    let Some(swarm) = swarm_opt else {
        return;
    };
    for (tier_label, storage_opt) in [
        ("Persistent Storage", swarm.persistence_storage.as_ref()),
        ("Cache Storage", swarm.cache_storage.as_ref()),
    ] {
        let Some(storage) = storage_opt else {
            continue;
        };
        for item in &storage.items {
            let key_matches = item.key.eq_ignore_ascii_case(target_entry_did)
                || exact_topic_key.is_some_and(|tk| item.key == tk);
            let did_matches = item
                .value
                .as_ref()
                .is_some_and(|v| v.did.eq_ignore_ascii_case(target_entry_did));
            if key_matches || did_matches {
                if !sources.iter().any(|s| s == tier_label) {
                    sources.push(tier_label.to_string());
                }
                if let Some(val) = &item.value {
                    record_kind.clone_from(&val.kind);
                    for raw_b64 in &val.data {
                        let decoded = decode_storage_element(raw_b64);
                        if !elements.contains(&decoded) {
                            elements.push(decoded);
                        }
                    }
                }
            }
        }
    }
}

/// Return current wall-clock time in Unix milliseconds.
pub fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

/// Built-in simulated node client (`--demo`) that produces live-updating telemetry, Chord DHT
/// ring state, peer credit metrics, and interactive command responses without requiring a running
/// daemon.
pub struct DemoNodeClient {
    started_at_ms: u128,
    tick: AtomicU64,
    peers: Mutex<Vec<PeerInfo>>,
    topics: Mutex<Vec<(String, String)>>,
    services: Mutex<Vec<String>>,
}

impl Default for DemoNodeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoNodeClient {
    /// Construct a new simulated [`DemoNodeClient`].
    pub fn new() -> Self {
        Self {
            started_at_ms: now_epoch_ms().saturating_sub(142_000),
            tick: AtomicU64::new(1),
            peers: Mutex::new(vec![
                PeerInfo {
                    did: "0x9f1c2e3a4b5d6e7f8091a2b3c4d5e6f708192a3b".to_string(),
                    state: "Connected".to_string(),
                },
                PeerInfo {
                    did: "0x4a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b".to_string(),
                    state: "Connected".to_string(),
                },
                PeerInfo {
                    did: "0x77c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6".to_string(),
                    state: "Connected".to_string(),
                },
            ]),
            topics: Mutex::new(vec![
                (
                    "rings-chat".to_string(),
                    "Welcome to Rings P2P overlay topic!".to_string(),
                ),
                (
                    "rings-chat".to_string(),
                    "Onion circuit hop established (3 relays)".to_string(),
                ),
            ]),
            services: Mutex::new(vec!["ipfs-gateway".to_string(), "turn-stun".to_string()]),
        }
    }
}

#[async_trait]
impl NodeClient for DemoNodeClient {
    async fn fetch_snapshot(&self, include_expired_directory: bool) -> Result<NodeSnapshot> {
        let step = self.tick.fetch_add(1, Ordering::Relaxed);
        let now_ms = now_epoch_ms();
        let peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();

        let sent = 240 + step * 7 + (step % 5) * 3;
        let received = 310 + step * 9 + (step % 4) * 4;
        let forwarded = 128 + step * 4 + (step % 3) * 2;
        let stored = 18 + step / 2;
        let failed = 2 + step / 15;

        let measurements = peers
            .iter()
            .enumerate()
            .map(|(i, p)| PeerMeasurementInfo {
                did: p.did.clone(),
                counters: PeerMeasurementCountersInfo {
                    connected: 1,
                    disconnected: 0,
                    sent: 80 + step * (3 + i as u64),
                    failed_to_send: u64::from(i == 2),
                    received: 95 + step * (4 + i as u64),
                    failed_to_receive: 0,
                },
                credit: Some(PeerCreditInfo {
                    bytes_sent_to_peer: (128_000 + step * 8_192) * (i as u64 + 1),
                    bytes_received_from_peer: (192_000 + step * 11_264) * (i as u64 + 1),
                    last_seen_seconds: u64::try_from(now_ms / 1_000).unwrap_or(0),
                    score: 4.2 + (i as f64) * 1.3,
                }),
                reliability: if i == 2 {
                    PeerReliabilityInfo::Unknown
                } else {
                    PeerReliabilityInfo::Healthy
                },
            })
            .collect::<Vec<_>>();

        let recent_messages = (0..10_u64)
            .map(|idx| {
                let seq = step.saturating_mul(2).saturating_add(idx);
                let (action, category, class) = match seq % 4 {
                    0 => ("sent", "application", "CustomBackendMessage"),
                    1 => ("received", "dht_control", "FindSuccessorReport"),
                    2 => ("forwarded", "e2e", "OnionCircuitFrame"),
                    _ => ("stored", "storage", "VNodeStoreAppend"),
                };
                RecentMessageEventDto {
                    sequence: seq,
                    observed_at_ms: now_ms.saturating_sub(u128::from((10 - idx) * 650)),
                    action: action.to_string(),
                    category: category.to_string(),
                    message_class: class.to_string(),
                    outcome: "succeeded".to_string(),
                }
            })
            .collect::<Vec<_>>();

        let local_did = "0x55a6b7c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e".to_string();
        let succ_did = peers
            .first()
            .map(|p| p.did.clone())
            .unwrap_or_else(|| local_did.clone());
        let pred_did = peers.last().map(|p| p.did.clone());
        let online_entry_did = crate::types::resolve_dht_key_did("online_nodes");
        let chat_entry_did = crate::types::resolve_dht_key_did("rings-chat");
        let svc_entry_did = crate::types::resolve_dht_key_did("ipfs-gateway");

        Ok(NodeSnapshot {
            did: Some(local_did.clone()),
            wire_version: Some(4),
            version: Some("1.0.0-pre.7".to_string()),
            swarm: Some(SwarmInfo {
                peers: peers.clone(),
                dht: Some(DhtInfo {
                    did: local_did.clone(),
                    successors: peers.iter().map(|p| p.did.clone()).collect(),
                    predecessor: pred_did,
                    finger_table_ranges: vec![
                        FingerTableRange {
                            did: Some(succ_did),
                            start: 0,
                            end: 31,
                        },
                        FingerTableRange {
                            did: peers.get(1).map(|p| p.did.clone()),
                            start: 32,
                            end: 95,
                        },
                        FingerTableRange {
                            did: peers.get(2).map(|p| p.did.clone()),
                            start: 96,
                            end: 159,
                        },
                    ],
                }),
                persistence_storage: Some(StorageInfo {
                    items: vec![
                        StorageItem {
                            key: online_entry_did.clone(),
                            value: Some(StorageValue {
                                did: online_entry_did,
                                kind: "Map".to_string(),
                                data: vec!["dGVzdA==".to_string(), "ZGF0YQ==".to_string()],
                            }),
                        },
                        StorageItem {
                            key: chat_entry_did.clone(),
                            value: Some(StorageValue {
                                did: chat_entry_did,
                                kind: "Append".to_string(),
                                data: vec!["bXNnMQ==".to_string()],
                            }),
                        },
                    ],
                }),
                cache_storage: Some(StorageInfo {
                    items: vec![StorageItem {
                        key: svc_entry_did.clone(),
                        value: Some(StorageValue {
                            did: svc_entry_did,
                            kind: "Append".to_string(),
                            data: vec!["cHJvdmlkZXI=".to_string()],
                        }),
                    }],
                }),
            }),
            peers,
            measurements,
            observability: Some(OperatorSnapshotDto {
                schema_version: 1,
                generated_at_ms: now_ms,
                process_started_at_ms: self.started_at_ms,
                counter_scope: "node_local_process_lifetime".to_string(),
                messages: MessageTotalsDto {
                    sent,
                    received,
                    forwarded,
                    stored,
                    failed,
                },
                recent_messages,
                session_key: SessionKeySnapshotDto {
                    valid: true,
                    created_at_ms: self.started_at_ms,
                    expires_at_ms: self.started_at_ms + 2_592_000_000,
                    remaining_ms: 2_310_000_000,
                    rotation_succeeded_total: 2,
                    rotation_failed_total: 0,
                    runtime_rotation_supported: true,
                },
                mailboxes: MailboxSnapshotDto {
                    registered: 3,
                    held_messages: 5,
                    stored_total: stored,
                },
                dht_lookups: DhtLookupSnapshotDto {
                    total: 84 + step * 2,
                    succeeded: 81 + step * 2,
                    failed: 1,
                    timed_out: 2,
                    in_flight: step % 2,
                    latency_sum_ms: 3_120 + u128::from(step * 42),
                    latency_count: 84 + step * 2,
                    latency_buckets: vec![
                        LatencyBucketDto {
                            le_ms: 5,
                            count: 14,
                        },
                        LatencyBucketDto {
                            le_ms: 10,
                            count: 32,
                        },
                        LatencyBucketDto {
                            le_ms: 25,
                            count: 58,
                        },
                        LatencyBucketDto {
                            le_ms: 50,
                            count: 72,
                        },
                        LatencyBucketDto {
                            le_ms: 100,
                            count: 80,
                        },
                        LatencyBucketDto {
                            le_ms: 250,
                            count: 82 + step,
                        },
                        LatencyBucketDto {
                            le_ms: 500,
                            count: 83 + step * 2,
                        },
                        LatencyBucketDto {
                            le_ms: 1_000,
                            count: 84 + step * 2,
                        },
                    ],
                },
                peer_ratings: Vec::new(),
                health: HealthSnapshotDto {
                    process_api_healthy: true,
                    admitted_peer_count: 3,
                    has_admitted_peer: true,
                    has_successor: true,
                    has_predecessor: true,
                    overlay_ready: true,
                },
            }),
            gateway: GatewayProbeState::Available(GatewayStatusDto {
                health: "active".to_string(),
                interface: Some("utun8".to_string()),
                capture_routes: vec!["198.18.0.0/15".to_string()],
                exit_availability: "available".to_string(),
                active_flows: 6,
                reason: None,
                last_flow_reset: None,
            }),
            online_nodes: vec![
                OnlineNodeDescriptorInfo {
                    did: local_did,
                    public_key: serde_json::Value::Null,
                    delegatee_public_key: serde_json::Value::Null,
                    node_type: OnlineNodeTypeInfo::Native,
                    network_id: 1,
                    storage_redundancy: 3,
                    dht_virtual_nodes: 16,
                    capabilities: OnlineNodeCapabilitiesInfo {
                        onion_relay: Some(serde_json::json!(1)),
                        holds_relay_mail: true,
                    },
                    endpoint_hint: Some("https://node1.rings.rs".to_string()),
                    started_at_ms: u64::try_from(self.started_at_ms).unwrap_or(0),
                    heartbeat_at_ms: u64::try_from(now_ms).unwrap_or(0),
                    fresh_for_ms: 60_000,
                    expires_at_ms: u64::try_from(now_ms + 300_000).unwrap_or(0),
                    version: "1.0.0-pre.7".to_string(),
                    signature: serde_json::Value::Null,
                },
                OnlineNodeDescriptorInfo {
                    did: "0x9f1c2e3a4b5d6e7f8091a2b3c4d5e6f708192a3b".to_string(),
                    public_key: serde_json::Value::Null,
                    delegatee_public_key: serde_json::Value::Null,
                    node_type: OnlineNodeTypeInfo::Native,
                    network_id: 1,
                    storage_redundancy: 3,
                    dht_virtual_nodes: 16,
                    capabilities: OnlineNodeCapabilitiesInfo {
                        onion_relay: Some(serde_json::json!(2)),
                        holds_relay_mail: true,
                    },
                    endpoint_hint: Some("https://relay2.rings.rs".to_string()),
                    started_at_ms: u64::try_from(self.started_at_ms).unwrap_or(0),
                    heartbeat_at_ms: u64::try_from(now_ms).unwrap_or(0),
                    fresh_for_ms: 60_000,
                    expires_at_ms: u64::try_from(now_ms + 300_000).unwrap_or(0),
                    version: "1.0.0-pre.7".to_string(),
                    signature: serde_json::Value::Null,
                },
            ],
            onion_exits: vec![OnionExitDescriptorInfo {
                did: "0x4a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b".to_string(),
                public_key: serde_json::Value::Null,
                delegatee_public_key: serde_json::Value::Null,
                process_epoch: serde_json::json!(1),
                node_type: OnlineNodeTypeInfo::Native,
                network_id: 1,
                service: "tcp".to_string(),
                policy: OnionExitPolicyInfo {
                    allowed_targets: vec!["*:443".to_string(), "*:80".to_string()],
                    denied_targets: vec!["127.0.0.0/8:*".to_string()],
                    max_sessions: 128,
                    max_bytes_per_minute: 104_857_600,
                },
                started_at_ms: u64::try_from(self.started_at_ms).unwrap_or(0),
                heartbeat_at_ms: u64::try_from(now_ms).unwrap_or(0),
                fresh_for_ms: 60_000,
                expires_at_ms: u64::try_from(now_ms + 300_000).unwrap_or(0),
                version: "1.0.0-pre.7".to_string(),
                signature: serde_json::Value::Null,
            }],
            online_nodes_error: None,
            onion_exits_error: None,
            include_expired_directory,
            warnings: Vec::new(),
            fetched_at_ms: now_ms,
            poll_latency_ms: 2,
        })
    }

    async fn connect_peer_via_http(
        &self,
        url: &str,
        _remote_api_token: Option<String>,
    ) -> Result<String> {
        let new_did = format!("0xdemo{:036x}", url.len());
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(PeerInfo {
                did: new_did.clone(),
                state: "Connected".to_string(),
            });
        Ok(new_did)
    }

    async fn connect_with_did(&self, did: &str) -> Result<()> {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !peers.iter().any(|p| p.did == did) {
            peers.push(PeerInfo {
                did: did.trim().to_string(),
                state: "Connected".to_string(),
            });
        }
        Ok(())
    }

    async fn connect_with_seed(&self, source: &str) -> Result<usize> {
        let parsed = load_seed_peers(source)?;
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for sp in &parsed {
            if !peers.iter().any(|p| p.did == sp.did) {
                peers.push(PeerInfo {
                    did: sp.did.clone(),
                    state: "Connected".to_string(),
                });
            }
        }
        Ok(parsed.len())
    }

    async fn disconnect(&self, did: &str) -> Result<()> {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|p| p.did != did.trim());
        Ok(())
    }

    async fn create_offer(&self, did: &str) -> Result<String> {
        Ok(format!("5KDemoOfferBase58PayloadForPeer{}", did.trim()))
    }

    async fn answer_offer(&self, offer: &str) -> Result<String> {
        Ok(format!("5KDemoAnswerBase58ReplyTo{}", offer.len()))
    }

    async fn accept_answer(&self, _answer: &str) -> Result<()> {
        Ok(())
    }

    async fn send_backend_message(
        &self,
        _destination_did: &str,
        _namespace: &str,
        _data: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn publish_topic_message(&self, topic: &str, data: &str) -> Result<()> {
        self.topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((topic.trim().to_string(), data.to_string()));
        Ok(())
    }

    async fn fetch_topic_messages(&self, topic: &str, skip: i64) -> Result<Vec<String>> {
        let skip_usize = usize::try_from(skip.max(0)).unwrap_or(0);
        let list = self
            .topics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(t, _)| t == topic.trim())
            .skip(skip_usize)
            .map(|(_, m)| m.clone())
            .collect();
        Ok(list)
    }

    async fn register_service(&self, name: &str) -> Result<()> {
        self.services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name.trim().to_string());
        Ok(())
    }

    async fn lookup_service(&self, _name: &str) -> Result<Vec<String>> {
        Ok(vec![
            "0x55a6b7c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e".to_string(),
            "0x9f1c2e3a4b5d6e7f8091a2b3c4d5e6f708192a3b".to_string(),
        ])
    }

    async fn peer_measurement(&self, did: &str) -> Result<Option<PeerMeasurementInfo>> {
        Ok(Some(PeerMeasurementInfo {
            did: did.trim().to_string(),
            counters: PeerMeasurementCountersInfo {
                connected: 1,
                disconnected: 0,
                sent: 142,
                failed_to_send: 0,
                received: 158,
                failed_to_receive: 0,
            },
            credit: Some(PeerCreditInfo {
                bytes_sent_to_peer: 262_144,
                bytes_received_from_peer: 524_288,
                last_seen_seconds: u64::try_from(now_epoch_ms() / 1_000).unwrap_or(0),
                score: 5.8,
            }),
            reliability: PeerReliabilityInfo::Healthy,
        }))
    }

    async fn dht_lookup_key(&self, key: &str) -> Result<DhtKeyLookupReport> {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err(TuiError::InvalidInput(
                "DHT lookup key cannot be empty".to_string(),
            ));
        }
        let target = DhtQueryTarget::parse(trimmed);
        let query_mode = target.mode_label().to_string();
        let target_did = target.target_entry_did().to_string();

        let mut elements = Vec::new();
        let mut sources = Vec::new();
        match &target {
            DhtQueryTarget::TopicKey { topic, .. } => {
                for (t, msg) in self
                    .topics
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                {
                    if t == topic {
                        elements.push(msg.clone());
                    }
                }
                if !elements.is_empty() {
                    sources.push("DHT Topic RPC (Network/Cache)".to_string());
                }
            }
            DhtQueryTarget::RawEntryDid { .. } => {}
        }

        if elements.is_empty() {
            sources.push("Persistent Storage".to_string());
            elements.push(format!(
                "vnode-record: key='{trimmed}' entry_did={target_did}"
            ));
        }

        Ok(DhtKeyLookupReport {
            query_key: trimmed.to_string(),
            query_mode,
            target_did,
            status: DhtLookupStatus::Found,
            sources,
            record_kind: "Data/Append".to_string(),
            elements,
            warning: None,
            duration_ms: 6,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shorthand_and_json_seed_inputs_and_surfaces_file_errors() {
        let peers =
            parse_seed_str("0x1111222233334444555566667777888899990000@http://127.0.0.1:50001")
                .expect("shorthand seed");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].url, "http://127.0.0.1:50001");

        let json_peers =
            parse_seed_str(r#"{"peers":[{"did":"0xaaaa","url":"https://node.example.com"}]}"#)
                .expect("json seed");
        assert_eq!(json_peers.len(), 1);
        assert_eq!(json_peers[0].did, "0xaaaa");

        let err = load_seed_peers("/nonexistent/rings-seed-file-that-does-not-exist.json")
            .expect_err("missing seed file must return file error");
        assert!(err.to_string().contains("failed to read seed file"));
    }

    #[test]
    fn http_node_client_rejects_invalid_bearer_tokens_at_construction() {
        assert!(HttpNodeClient::new("http://127.0.0.1:50000", Some("valid-token".into())).is_ok());
        assert!(HttpNodeClient::new("http://127.0.0.1:50000", Some("bad\ntoken".into())).is_err());
        assert!(HttpNodeClient::new("http://127.0.0.1:50000", Some("   ".into())).is_err());
    }

    #[tokio::test]
    async fn read_bounded_json_enforces_content_length_chunked_exact_and_malformed_bounds() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        async fn serve_once(raw_http_response: Vec<u8>) -> String {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind local test listener");
            let addr = listener.local_addr().expect("listener local addr");
            tokio::spawn(async move {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut req_buf = [0u8; 1024];
                    let _ = stream.read(&mut req_buf).await;
                    let _ = stream.write_all(&raw_http_response).await;
                    let _ = stream.shutdown().await;
                }
            });
            format!("http://{addr}/status")
        }

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build test reqwest client");
        let max_bytes = 64_usize;

        // 1. Oversized Content-Length header (> max_bytes) is rejected immediately.
        let oversized_header_url = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                max_bytes + 1,
                "a".repeat(max_bytes + 1)
            )
            .into_bytes(),
        )
        .await;
        let resp = client
            .get(&oversized_header_url)
            .send()
            .await
            .expect("send oversized header req");
        let err = read_bounded_json::<serde_json::Value>(resp, max_bytes)
            .await
            .expect_err("oversized Content-Length must fail");
        assert!(err.to_string().contains("exceeds limit"));

        // 2. Chunked body without Content-Length crossing max_bytes (65 bytes > 64) is rejected.
        let chunk1 = "x".repeat(40);
        let chunk2 = "y".repeat(25);
        let chunked_over_url = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{}\r\n{:X}\r\n{}\r\n0\r\n\r\n",
                chunk1.len(),
                chunk1,
                chunk2.len(),
                chunk2
            )
            .into_bytes(),
        )
        .await;
        let resp = client
            .get(&chunked_over_url)
            .send()
            .await
            .expect("send chunked over req");
        let err = read_bounded_json::<serde_json::Value>(resp, max_bytes)
            .await
            .expect_err("chunked body crossing limit must fail");
        assert!(err.to_string().contains("exceeded limit"));

        // 3. Exact-limit body (len == max_bytes == 64) succeeds and decodes.
        let prefix = r#"{"ok":""#;
        let suffix = r#""}"#;
        let pad_len = max_bytes - prefix.len() - suffix.len();
        let exact_body = format!("{prefix}{}{suffix}", "z".repeat(pad_len));
        assert_eq!(exact_body.len(), max_bytes);
        let exact_url = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                exact_body.len(),
                exact_body
            )
            .into_bytes(),
        )
        .await;
        let resp = client.get(&exact_url).send().await.expect("send exact req");
        let val = read_bounded_json::<serde_json::Value>(resp, max_bytes)
            .await
            .expect("exact-limit JSON must succeed");
        assert_eq!(
            val.get("ok").and_then(serde_json::Value::as_str),
            Some("z".repeat(pad_len).as_str())
        );

        // 4. Malformed / truncated JSON within limit is rejected with a JSON decode error.
        let malformed_body = r#"{"ok":"unterminated"#;
        let malformed_url = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                malformed_body.len(),
                malformed_body
            )
            .into_bytes(),
        )
        .await;
        let resp = client
            .get(&malformed_url)
            .send()
            .await
            .expect("send malformed req");
        let err = read_bounded_json::<serde_json::Value>(resp, max_bytes)
            .await
            .expect_err("malformed JSON must fail");
        assert!(err
            .to_string()
            .contains("failed to decode status JSON response"));
    }
}
