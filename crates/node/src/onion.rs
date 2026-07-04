#![warn(missing_docs)]
//! Application-layer onion routing directory and route selection.
//!
//! This module deliberately sits in `rings-node`, not `rings-core`: Chord
//! remains the storage and discovery substrate, while onion exit policy is an
//! application protocol decision.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
use rings_core::measure::PeerQuality;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::decode_descriptor;
use crate::descriptor::encode_descriptor;
use crate::descriptor::latest_valid_by_did;
use crate::descriptor::SignedDescriptor;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

/// DHT topic used for application-layer onion exit descriptors.
pub const ONION_EXITS_TOPIC: &str = "onion_exits";

/// Default number of DID hops in a production onion route, including the exit.
pub const DEFAULT_ONION_ROUTE_HOPS: usize = 3;

/// Capability label for nodes willing to relay onion cells.
pub const ONION_RELAY_CAPABILITY: &str = "onion-relay";

/// Capability label for nodes willing to publish onion exit policy.
pub const ONION_EXIT_CAPABILITY: &str = "onion-exit";

const DEFAULT_ONION_EXIT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_ONION_EXIT_TTL_SECS: u64 = 90;

/// Default onion-exit registry heartbeat interval in seconds.
pub(crate) const fn default_onion_exit_heartbeat_interval_secs() -> u64 {
    DEFAULT_ONION_EXIT_HEARTBEAT_INTERVAL_SECS
}

/// Default onion-exit registry descriptor TTL in seconds.
pub(crate) const fn default_onion_exit_ttl_secs() -> u64 {
    DEFAULT_ONION_EXIT_TTL_SECS
}

/// Default onion relay advertisement enablement.
pub(crate) const fn default_advertise_onion_relay() -> bool {
    false
}

/// Default onion exit advertisement enablement.
pub(crate) const fn default_advertise_onion_exit() -> bool {
    false
}

/// Default exit services. It is only published when onion-exit advertisement is enabled.
pub fn default_onion_exit_services() -> Vec<OnionExitService> {
    vec![OnionExitService::https(), OnionExitService::tcp()]
}

/// Default exit policy. It is intentionally closed until the operator configures targets.
pub fn default_onion_exit_policy() -> OnionExitPolicy {
    OnionExitPolicy::default()
}

/// Validate onion-exit registration scheduling.
pub(crate) fn validate_onion_exit_registration_timing(
    advertise_exit: bool,
    heartbeat_interval: Duration,
    ttl: Duration,
) -> Result<()> {
    if advertise_exit && heartbeat_interval >= ttl {
        return Err(Error::InvalidConfig(format!(
            "onion_exit_heartbeat_interval ({heartbeat_interval:?}) must be less than onion_exit_ttl ({ttl:?}) when advertise_onion_exit is enabled"
        )));
    }
    Ok(())
}

/// Application transport exposed by an onion exit service.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum OnionExitTransport {
    /// Native TCP service.
    Tcp,
    /// Native UDP service.
    Udp,
    /// Browser/WebTransport-backed service.
    WebTransport,
    /// Protocol-specific request/response service.
    RequestResponse,
    /// Browser HTTPS fetch service.
    Https,
}

/// One named service offered by an onion exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitService {
    /// Service name advertised to route builders.
    pub name: String,
    /// Transport used by this service.
    pub transport: OnionExitTransport,
}

impl OnionExitService {
    /// Return the standard browser HTTPS exit service.
    pub fn https() -> Self {
        Self {
            name: "https".to_string(),
            transport: OnionExitTransport::Https,
        }
    }

    /// Return the standard native TCP exit service.
    pub fn tcp() -> Self {
        Self {
            name: "tcp".to_string(),
            transport: OnionExitTransport::Tcp,
        }
    }

    /// Return whether this service has the requested name.
    pub fn has_name(&self, service: &str) -> bool {
        self.name == service
    }
}

/// Signed policy fields for an onion exit.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitPolicy {
    /// Target allow-list entries understood by the exit implementation.
    pub allowed_targets: Vec<String>,
    /// Target deny-list entries understood by the exit implementation.
    pub denied_targets: Vec<String>,
    /// Maximum concurrent circuits this exit wants to serve. `0` means unspecified.
    pub max_circuits: u32,
    /// Maximum streams per circuit. `0` means unspecified.
    pub max_streams_per_circuit: u32,
    /// Maximum bytes per minute. `0` means unspecified.
    pub max_bytes_per_minute: u64,
}

/// Descriptor fields covered by the onion-exit signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitDescriptorBody {
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Services this exit is willing to expose.
    pub services: Vec<OnionExitService>,
    /// Signed exit policy.
    pub policy: OnionExitPolicy,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// Heartbeat timestamp in milliseconds since Unix epoch.
    pub heartbeat_at_ms: u128,
    /// Expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
}

impl OnionExitDescriptorBody {
    fn validate_signer(&self, session_sk: &SessionSk) -> CoreResult<()> {
        if self.public_key.did() != self.did || session_sk.account_did() != self.did {
            return Err(CoreError::InvalidMessage(
                "onion exit descriptor DID/public key/session mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        OnionExitDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            services: &self.services,
            policy: &self.policy,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version.as_str(),
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        self.body_ref().signing_data()
    }
}

#[derive(Serialize)]
struct OnionExitDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    services: &'a [OnionExitService],
    policy: &'a OnionExitPolicy,
    started_at_ms: u128,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    version: &'a str,
}

impl OnionExitDescriptorBodyRef<'_> {
    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        bincode::serialize(self).map_err(CoreError::BincodeSerialize)
    }
}

/// Signed descriptor published by onion exits.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitDescriptor {
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Services this exit is willing to expose.
    pub services: Vec<OnionExitService>,
    /// Signed exit policy.
    pub policy: OnionExitPolicy,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// Heartbeat timestamp in milliseconds since Unix epoch.
    pub heartbeat_at_ms: u128,
    /// Expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
    /// Signature covering every descriptor field above.
    pub signature: MessageVerification,
}

impl OnionExitDescriptor {
    /// Create and sign an onion-exit descriptor.
    pub fn new_signed(body: OnionExitDescriptorBody, session_sk: &SessionSk) -> CoreResult<Self> {
        body.validate_signer(session_sk)?;
        let signature = MessageVerification::new(&body.signing_data()?, session_sk)?;
        Ok(Self {
            did: body.did,
            public_key: body.public_key,
            node_type: body.node_type,
            network_id: body.network_id,
            services: body.services,
            policy: body.policy,
            started_at_ms: body.started_at_ms,
            heartbeat_at_ms: body.heartbeat_at_ms,
            expires_at_ms: body.expires_at_ms,
            version: body.version,
            signature,
        })
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            node_type,
            network_id,
            services,
            policy,
            started_at_ms,
            heartbeat_at_ms,
            expires_at_ms,
            version,
            signature: _,
        } = self;

        OnionExitDescriptorBodyRef {
            did: *did,
            public_key,
            node_type,
            network_id: *network_id,
            services,
            policy,
            started_at_ms: *started_at_ms,
            heartbeat_at_ms: *heartbeat_at_ms,
            expires_at_ms: *expires_at_ms,
            version: version.as_str(),
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        self.body_ref().signing_data()
    }

    /// Return whether this descriptor belongs to `network_id`.
    pub const fn matches_network(&self, network_id: u32) -> bool {
        self.network_id == network_id
    }

    /// Return whether this descriptor offers `service`.
    pub fn offers_service(&self, service: &str) -> bool {
        self.services
            .iter()
            .any(|candidate| candidate.has_name(service))
    }

    /// Verify the descriptor signature and DID/public-key binding.
    pub fn verify_signature(&self) -> bool {
        self.descriptor_verify_signature()
    }

    /// Returns whether this descriptor is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_expired_at(now_ms)
    }

    /// Returns whether this descriptor has a valid signature and is not expired.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_live_at(now_ms)
    }

    /// Select the newest valid onion-exit descriptor per DID.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_did(descriptors, now_ms, include_expired)
    }
}

impl SignedDescriptor for OnionExitDescriptor {
    fn descriptor_did(&self) -> Did {
        self.did
    }

    fn descriptor_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn descriptor_signature(&self) -> &MessageVerification {
        &self.signature
    }

    fn descriptor_heartbeat_at_ms(&self) -> u128 {
        self.heartbeat_at_ms
    }

    fn descriptor_expires_at_ms(&self) -> u128 {
        self.expires_at_ms
    }

    fn descriptor_signing_data(&self) -> CoreResult<Vec<u8>> {
        self.signing_data()
    }
}

impl Encoder for OnionExitDescriptor {
    fn encode(&self) -> CoreResult<Encoded> {
        encode_descriptor(self)
    }
}

impl Decoder for OnionExitDescriptor {
    fn from_encoded(encoded: &Encoded) -> CoreResult<Self> {
        decode_descriptor(encoded)
    }
}

/// Periodic node-layer registration for onion exit policy.
#[derive(Clone, Debug)]
pub struct OnionExitRegistration {
    heartbeat_interval: Duration,
    ttl: Duration,
    node_type: OnlineNodeType,
    started_at_ms: u128,
    services: Vec<OnionExitService>,
    policy: OnionExitPolicy,
    publisher: DhtRegistrationPublisher,
}

impl OnionExitRegistration {
    /// Create an onion-exit registration task.
    pub fn new(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        services: Vec<OnionExitService>,
        policy: OnionExitPolicy,
    ) -> Self {
        Self {
            heartbeat_interval,
            ttl,
            node_type,
            started_at_ms: get_epoch_ms(),
            services,
            policy,
            publisher: DhtRegistrationPublisher::new(ONION_EXITS_TOPIC),
        }
    }

    /// Validate this registration's periodic schedule when it is enabled.
    pub fn validate_enabled_schedule(&self) -> Result<()> {
        if self.heartbeat_interval >= self.ttl {
            return Err(Error::InvalidConfig(format!(
                "onion_exit_heartbeat_interval ({:?}) must be less than onion_exit_ttl ({:?})",
                self.heartbeat_interval, self.ttl
            )));
        }
        Ok(())
    }

    /// Build this node's signed onion-exit descriptor at `now_ms`.
    pub fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
    ) -> Result<OnionExitDescriptor> {
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: context.did(),
                public_key: context.account_verification_pubkey()?,
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                services: self.services.clone(),
                policy: self.policy.clone(),
                started_at_ms: self.started_at_ms,
                heartbeat_at_ms: now_ms,
                expires_at_ms: now_ms + self.ttl.as_millis(),
                version: crate::util::build_version(),
            },
            context.session_sk(),
        )
        .map_err(Error::CoreError)
    }

    /// Publish this node's signed onion-exit descriptor.
    pub async fn publish_descriptor(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<OnionExitDescriptor> {
        let now_ms = get_epoch_ms();
        let descriptor = self.descriptor_at(context, now_ms)?;
        let encoded = descriptor.encode().map_err(Error::CoreError)?;
        self.publisher.publish(context, encoded).await?;
        Ok(descriptor)
    }

    /// Decode onion-exit descriptors from a DHT entry.
    pub fn descriptors_from_entry(
        entry: &rings_core::prelude::entry::Entry,
    ) -> Vec<OnionExitDescriptor> {
        entry
            .data
            .iter()
            .filter_map(|value| value.decode::<OnionExitDescriptor>().ok())
            .collect()
    }
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl RegistrationTask for OnionExitRegistration {
    fn name(&self) -> &'static str {
        "onion-exit"
    }

    fn interval(&self) -> Duration {
        self.heartbeat_interval
    }

    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
        self.publish_descriptor(context).await.map(|_| ())
    }
}

/// Route-building request for an onion circuit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRouteRequest {
    /// Exit service required by the route.
    pub service: String,
    /// Desired hop count including the exit. `0` uses [`DEFAULT_ONION_ROUTE_HOPS`].
    pub hop_count: usize,
    /// Whether a route may be shorter than `hop_count` when the network is too small.
    pub allow_short_paths: bool,
}

impl OnionRouteRequest {
    fn target_hop_count(&self) -> usize {
        if self.hop_count == 0 {
            DEFAULT_ONION_ROUTE_HOPS
        } else {
            self.hop_count
        }
    }
}

/// Selected onion route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRoute {
    /// Exit service requested by the route.
    pub service: String,
    /// Ordered DIDs, ending with the exit DID.
    pub hops: Vec<Did>,
    /// Signed descriptor for the selected exit.
    pub exit: OnionExitDescriptor,
}

impl OnionRoute {
    /// Return the selected exit DID.
    pub fn exit_did(&self) -> Did {
        self.exit.did
    }
}

pub(crate) trait RouteEntropy {
    fn next_u64(&mut self) -> u64;
}

pub(crate) struct SystemRouteEntropy;

impl SystemRouteEntropy {
    pub(crate) const fn new() -> Self {
        Self
    }
}

#[cfg(not(feature = "browser"))]
impl RouteEntropy for SystemRouteEntropy {
    fn next_u64(&mut self) -> u64 {
        rand::random()
    }
}

#[cfg(feature = "browser")]
impl RouteEntropy for SystemRouteEntropy {
    fn next_u64(&mut self) -> u64 {
        let upper = (js_sys::Math::random() * (u32::MAX as f64)) as u64;
        let lower = (js_sys::Math::random() * (u32::MAX as f64)) as u64;
        (upper << 32) | lower
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OnionRouteCandidates {
    relays: Vec<Did>,
    exits: Vec<OnionExitDescriptor>,
}

impl OnionRouteCandidates {
    pub(crate) fn from_validated_descriptors(
        local: Did,
        network_id: u32,
        service: &str,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        exits: impl IntoIterator<Item = OnionExitDescriptor>,
    ) -> Self {
        let relays = online_nodes
            .into_iter()
            .filter(|descriptor| descriptor.matches_network(network_id))
            .filter(has_onion_relay_capability)
            .map(|descriptor| descriptor.did)
            .filter(|did| *did != local)
            .collect::<BTreeSet<_>>();

        let exits = exits
            .into_iter()
            .filter(|descriptor| descriptor.matches_network(network_id))
            .filter(|descriptor| descriptor.offers_service(service))
            .filter(|descriptor| descriptor.did != local)
            .collect::<Vec<_>>();

        Self {
            relays: relays.into_iter().collect(),
            exits,
        }
    }
}

/// Select an onion route from live presence and exit descriptors.
///
/// Invariant: the returned hop list contains no duplicate DID and always ends
/// in a descriptor from the exit registry.
pub fn select_onion_route(
    local: Did,
    network_id: u32,
    now_ms: u128,
    request: &OnionRouteRequest,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Result<OnionRoute> {
    let service = request.service.trim();
    let candidates = OnionRouteCandidates {
        relays: eligible_relay_dids(network_id, now_ms, local, online_nodes)
            .into_iter()
            .collect(),
        exits: eligible_exits(network_id, now_ms, service, exits)
            .into_iter()
            .filter(|descriptor| descriptor.did != local)
            .collect(),
    };
    select_onion_route_from_candidates(
        request,
        candidates,
        qualities,
        &mut SystemRouteEntropy::new(),
    )
}

pub(crate) fn select_onion_route_from_candidates(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
) -> Result<OnionRoute> {
    let service = request.service.trim();
    if service.is_empty() {
        return Err(Error::OnionRouteError(
            "onion route service must not be empty".to_string(),
        ));
    }

    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let mut exit_candidates = candidates.exits;
    let exit_dids = exit_candidates
        .iter()
        .map(|descriptor| descriptor.did)
        .collect::<Vec<_>>();
    let exit_index =
        pick_weighted_index(&exit_dids, &quality_by_did, entropy).ok_or_else(|| {
            Error::OnionRouteError(format!("no live onion exit offers service {service:?}"))
        })?;
    let exit = exit_candidates.remove(exit_index);
    let exit_did = exit.did;

    let mut relay_candidates = candidates
        .relays
        .into_iter()
        .filter(|did| *did != exit_did)
        .collect::<Vec<_>>();
    let relay_hops_needed = request.target_hop_count().saturating_sub(1);
    let mut selected_relays = Vec::with_capacity(relay_hops_needed);
    while selected_relays.len() < relay_hops_needed {
        let Some(next_index) = pick_weighted_index(&relay_candidates, &quality_by_did, entropy)
        else {
            break;
        };
        selected_relays.push(relay_candidates.remove(next_index));
    }

    if selected_relays.len() < relay_hops_needed && !request.allow_short_paths {
        return Err(Error::OnionRouteError(format!(
            "not enough relay candidates for {}-hop onion route",
            request.target_hop_count()
        )));
    }

    let mut hops = selected_relays;
    hops.push(exit_did);
    debug_assert!(!has_duplicate_dids(&hops));

    Ok(OnionRoute {
        service: service.to_string(),
        hops,
        exit,
    })
}

fn pick_weighted_index(
    dids: &[Did],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<usize> {
    let total_weight = dids
        .iter()
        .map(|did| quality_weight(quality_by_did.get(did).copied()))
        .sum::<u64>();
    if total_weight == 0 {
        return None;
    }

    let mut roll = entropy.next_u64() % total_weight;
    for (index, did) in dids.iter().enumerate() {
        let weight = quality_weight(quality_by_did.get(did).copied());
        if roll < weight {
            return Some(index);
        }
        roll -= weight;
    }
    None
}

fn quality_weight(quality: Option<PeerQuality>) -> u64 {
    match quality {
        Some(PeerQuality::Healthy) => 8,
        Some(PeerQuality::Unknown) | None => 4,
        Some(PeerQuality::Degraded) => 1,
    }
}

fn eligible_exits(
    network_id: u32,
    now_ms: u128,
    service: &str,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
) -> Vec<OnionExitDescriptor> {
    OnionExitDescriptor::latest_valid_by_did(exits, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_network(network_id))
        .filter(|descriptor| descriptor.offers_service(service))
        .collect()
}

fn eligible_relay_dids(
    network_id: u32,
    now_ms: u128,
    local: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> BTreeSet<Did> {
    OnlineNodeDescriptor::latest_valid_by_did(online_nodes, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_network(network_id))
        .filter(has_onion_relay_capability)
        .map(|descriptor| descriptor.did)
        .filter(|did| *did != local)
        .collect()
}

fn has_onion_relay_capability(descriptor: &OnlineNodeDescriptor) -> bool {
    descriptor
        .capabilities
        .iter()
        .any(|capability| capability == ONION_RELAY_CAPABILITY)
}

fn has_duplicate_dids(hops: &[Did]) -> bool {
    let mut seen = BTreeSet::new();
    hops.iter().any(|did| !seen.insert(*did))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use super::*;
    use crate::online::OnlineNodeDescriptorBody;

    fn service(name: &str) -> OnionExitService {
        OnionExitService {
            name: name.to_string(),
            transport: OnionExitTransport::Tcp,
        }
    }

    fn signed_exit_at(
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
    ) -> CoreResult<OnionExitDescriptor> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key: session_sk.session().account_verification_pubkey()?,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy {
                    allowed_targets: vec!["example.com:443".to_string()],
                    denied_targets: vec![],
                    max_circuits: 16,
                    max_streams_per_circuit: 4,
                    max_bytes_per_minute: 1024,
                },
                started_at_ms: 1,
                heartbeat_at_ms,
                expires_at_ms,
                version: "test".to_string(),
            },
            &session_sk,
        )
    }

    fn online_node_at(
        session_sk: &SessionSk,
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
    ) -> CoreResult<OnlineNodeDescriptor> {
        online_node_at_with_capabilities(session_sk, heartbeat_at_ms, expires_at_ms, vec![
            ONION_RELAY_CAPABILITY.to_string(),
        ])
    }

    fn online_node_at_with_capabilities(
        session_sk: &SessionSk,
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
        capabilities: Vec<String>,
    ) -> CoreResult<OnlineNodeDescriptor> {
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: session_sk.account_did(),
                public_key: session_sk.session().account_verification_pubkey()?,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                capabilities,
                endpoint_hint: None,
                started_at_ms: 1,
                heartbeat_at_ms,
                expires_at_ms,
                version: "test".to_string(),
            },
            session_sk,
        )
    }

    fn node_key() -> CoreResult<SessionSk> {
        SessionSk::new_with_seckey(&SecretKey::random())
    }

    struct FixedEntropy {
        values: VecDeque<u64>,
    }

    impl FixedEntropy {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: values.into_iter().collect(),
            }
        }
    }

    impl RouteEntropy for FixedEntropy {
        fn next_u64(&mut self) -> u64 {
            self.values.pop_front().unwrap_or(0)
        }
    }

    #[test]
    fn exit_descriptor_signature_covers_policy() -> CoreResult<()> {
        let mut descriptor = signed_exit_at(20, 100)?;
        assert!(descriptor.verify_signature());

        descriptor.policy.max_circuits = 32;

        assert!(!descriptor.verify_signature());
        Ok(())
    }

    #[test]
    fn latest_valid_by_did_filters_expired_and_keeps_newest() -> CoreResult<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        let public_key = session_sk.session().account_verification_pubkey()?;

        let older = OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key: public_key.clone(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy::default(),
                started_at_ms: 1,
                heartbeat_at_ms: 10,
                expires_at_ms: 100,
                version: "old".to_string(),
            },
            &session_sk,
        )?;
        let newer = OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy::default(),
                started_at_ms: 1,
                heartbeat_at_ms: 20,
                expires_at_ms: 100,
                version: "new".to_string(),
            },
            &session_sk,
        )?;
        let other_live = signed_exit_at(25, 100)?;
        let expired = signed_exit_at(30, 40)?;

        let descriptors = OnionExitDescriptor::latest_valid_by_did(
            vec![
                older.clone(),
                newer.clone(),
                other_live.clone(),
                expired.clone(),
            ],
            50,
            false,
        );

        assert_eq!(descriptors.len(), 2);
        assert!(descriptors.iter().any(|descriptor| descriptor == &newer));
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor == &other_live));

        let with_expired = OnionExitDescriptor::latest_valid_by_did(
            vec![older, newer, other_live, expired],
            50,
            true,
        );
        assert_eq!(with_expired.len(), 3);
        Ok(())
    }

    #[test]
    fn route_builder_uses_presence_relays_and_exit_registry() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let first_relay = node_key().map_err(Error::CoreError)?;
        let second_relay = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let online = vec![
            online_node_at(&first_relay, 20, 100).map_err(Error::CoreError)?,
            online_node_at(&second_relay, 20, 100).map_err(Error::CoreError)?,
        ];
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 3,
            allow_short_paths: false,
        };

        let route = select_onion_route(
            local,
            1,
            50,
            &request,
            online,
            vec![exit.clone()],
            Vec::new(),
        )?;

        assert_eq!(route.hops.len(), 3);
        assert_eq!(route.exit_did(), exit.did);
        assert_eq!(route.hops.last().copied(), Some(exit.did));
        assert_ne!(route.hops.first().copied(), Some(exit.did));
        Ok(())
    }

    #[test]
    fn route_builder_rejects_too_short_production_route() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let relay = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 3,
            allow_short_paths: false,
        };

        let result = select_onion_route(
            local,
            1,
            50,
            &request,
            vec![online_node_at(&relay, 20, 100).map_err(Error::CoreError)?],
            vec![exit],
            Vec::new(),
        );

        assert!(
            matches!(result, Err(Error::OnionRouteError(message)) if message.contains("not enough relay"))
        );
        Ok(())
    }

    #[test]
    fn route_builder_rejects_nodes_without_relay_capability() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let relay = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 2,
            allow_short_paths: false,
        };

        let result = select_onion_route(
            local,
            1,
            50,
            &request,
            vec![online_node_at_with_capabilities(&relay, 20, 100, vec![])
                .map_err(Error::CoreError)?],
            vec![exit],
            Vec::new(),
        );

        assert!(
            matches!(result, Err(Error::OnionRouteError(message)) if message.contains("not enough relay"))
        );
        Ok(())
    }

    #[test]
    fn route_builder_samples_relays_by_quality_weight() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let degraded = node_key().map_err(Error::CoreError)?;
        let healthy = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 2,
            allow_short_paths: false,
        };
        let candidates = OnionRouteCandidates {
            relays: vec![degraded.account_did(), healthy.account_did()],
            exits: vec![exit],
        };
        let mut entropy = FixedEntropy::new([0, 1]);

        let route = select_onion_route_from_candidates(
            &request,
            candidates,
            vec![
                (degraded.account_did(), PeerQuality::Degraded),
                (healthy.account_did(), PeerQuality::Healthy),
            ],
            &mut entropy,
        )?;

        assert_eq!(route.hops.first().copied(), Some(healthy.account_did()));
        assert_ne!(route.hops.first().copied(), Some(local));
        Ok(())
    }

    #[test]
    fn route_builder_entropy_can_select_second_unknown_relay() -> Result<()> {
        let first = node_key().map_err(Error::CoreError)?.account_did();
        let second = node_key().map_err(Error::CoreError)?.account_did();
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 2,
            allow_short_paths: false,
        };
        let mut relay_dids = vec![first, second];
        relay_dids.sort();
        let second_sorted = relay_dids
            .get(1)
            .copied()
            .ok_or_else(|| Error::OnionRouteError("missing second relay".to_string()))?;
        let candidates = OnionRouteCandidates {
            relays: relay_dids,
            exits: vec![exit],
        };
        let mut entropy = FixedEntropy::new([0, 4]);

        let route =
            select_onion_route_from_candidates(&request, candidates, Vec::new(), &mut entropy)?;

        assert_eq!(route.hops.first().copied(), Some(second_sorted));
        Ok(())
    }
}
