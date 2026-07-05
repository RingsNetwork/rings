#![warn(missing_docs)]
//! Application-layer circuit directory and route selection.
//!
//! This module deliberately sits in `rings-node`, not `rings-core`: Chord
//! remains the storage and discovery substrate, while exit policy is an
//! application protocol decision.
//!
//! The current data plane selects route-aware circuits and exit policies. It does not yet provide
//! layered onion encryption; see [`circuit::ONION_CIRCUIT_SECURITY`].

use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
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
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeType;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

pub mod circuit;
pub mod route;
pub mod target;
#[cfg(feature = "node")]
pub mod tcp;

pub use route::select_onion_route;
pub(crate) use route::select_onion_route_from_candidates;
pub use route::OnionRoute;
pub(crate) use route::OnionRouteCandidates;
pub use route::OnionRouteHop;
pub use route::OnionRouteRequest;
pub(crate) use route::SystemRouteEntropy;
pub use route::DEFAULT_ONION_ROUTE_HOPS;
pub use target::OnionProxyTarget;

/// DHT topic used for application-layer onion exit descriptors.
pub const ONION_EXITS_TOPIC: &str = "onion_exits";

/// Capability label for nodes willing to relay onion cells.
pub const ONION_RELAY_CAPABILITY: &str = "onion-relay";

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

/// Default native exit services. It is only published when onion-exit advertisement is enabled.
pub fn default_onion_exit_services() -> Vec<OnionExitService> {
    vec![OnionExitService::tcp()]
}

/// Browser HTTPS-only onion-exit service set.
pub fn https_onion_exit_services() -> Vec<OnionExitService> {
    vec![OnionExitService::https()]
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
    /// Browser/application-layer HTTPS proxy service.
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
    /// Return a named exit service with an explicit transport.
    pub fn new(name: impl Into<String>, transport: OnionExitTransport) -> Self {
        Self {
            name: name.into(),
            transport,
        }
    }

    /// Return the standard browser HTTPS exit service.
    pub fn https() -> Self {
        Self::new("https", OnionExitTransport::Https)
    }

    /// Return the standard native TCP exit service.
    pub fn tcp() -> Self {
        Self::new("tcp", OnionExitTransport::Tcp)
    }

    /// Return whether this service has the requested name.
    pub fn has_name(&self, service: &str) -> bool {
        self.name == service
    }

    /// Return whether this service has the requested name and transport.
    pub fn matches(&self, service: &str, transport: OnionExitTransport) -> bool {
        self.has_name(service) && self.transport == transport
    }

    /// Return whether this service satisfies a route request for `service`.
    ///
    /// Built-in service names reserve their transport class. Custom service names remain
    /// application-defined and match by name.
    pub fn matches_route_service(&self, service: &str) -> bool {
        match Self::reserved_transport(service) {
            Some(transport) => self.matches(service, transport),
            None => self.has_name(service),
        }
    }

    /// Return the reserved transport for a built-in service name.
    pub fn reserved_transport(service: &str) -> Option<OnionExitTransport> {
        match service {
            "tcp" => Some(OnionExitTransport::Tcp),
            "https" => Some(OnionExitTransport::Https),
            _ => None,
        }
    }
}

/// Signed policy fields for an onion exit.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitPolicy {
    /// Target allow-list entries understood by the exit implementation. Empty means closed.
    pub allowed_targets: Vec<String>,
    /// Target deny-list entries understood by the exit implementation. Deny entries override allows.
    pub denied_targets: Vec<String>,
    /// Maximum concurrent circuits this exit wants to serve. `0` means unspecified.
    pub max_circuits: u32,
    /// Maximum streams per circuit. `0` means unspecified.
    pub max_streams_per_circuit: u32,
    /// Maximum bytes per minute. `0` means unspecified.
    pub max_bytes_per_minute: u64,
}

impl OnionExitPolicy {
    /// Return whether this policy denies every exit target.
    pub fn is_closed(&self) -> bool {
        !self.has_valid_allowed_target()
    }

    /// Return whether this policy has at least one syntactically valid allowed target.
    pub fn has_valid_allowed_target(&self) -> bool {
        self.allowed_targets
            .iter()
            .any(|target| canonical_exit_target(target).is_some())
    }

    /// Validate target lists for an advertised onion exit.
    pub fn validate_targets(&self) -> Result<()> {
        if let Some(target) = self
            .allowed_targets
            .iter()
            .find(|target| canonical_exit_target(target).is_none())
        {
            return Err(Error::InvalidConfig(format!(
                "invalid onion exit allowed target {target:?}; expected host:port"
            )));
        }
        if let Some(target) = self
            .denied_targets
            .iter()
            .find(|target| canonical_exit_target(target).is_none())
        {
            return Err(Error::InvalidConfig(format!(
                "invalid onion exit denied target {target:?}; expected host:port"
            )));
        }
        if self.is_closed() {
            return Err(Error::InvalidConfig(
                "advertise_onion_exit requires at least one valid onion_exit_policy allowed target"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Return whether `target` is admitted by this policy's allow-list.
    pub fn allows_target(&self, target: &str) -> bool {
        let Some(target) = canonical_exit_target(target) else {
            return false;
        };
        if self.is_closed() {
            return false;
        }
        if self
            .denied_targets
            .iter()
            .filter_map(|denied| canonical_exit_target(denied))
            .any(|denied| denied == target)
        {
            return false;
        }
        self.allowed_targets
            .iter()
            .filter_map(|allowed| canonical_exit_target(allowed))
            .any(|allowed| allowed == target)
    }
}

fn canonical_exit_target(target: &str) -> Option<String> {
    OnionProxyTarget::parse_authority(target)
        .ok()
        .map(|target| target.authority())
}

/// Descriptor fields covered by the onion-exit signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitDescriptorBody {
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Session public key used for encrypted onion exit frames.
    pub session_public_key: PublicKey<33>,
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
    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        OnionExitDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            session_public_key: &self.session_public_key,
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

impl SignedDescriptorBody for OnionExitDescriptorBody {
    type Descriptor = OnionExitDescriptor;

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_signing_data(&self) -> CoreResult<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        OnionExitDescriptor {
            did: self.did,
            public_key: self.public_key,
            session_public_key: self.session_public_key,
            node_type: self.node_type,
            network_id: self.network_id,
            services: self.services,
            policy: self.policy,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version,
            signature,
        }
    }
}

#[derive(Serialize)]
struct OnionExitDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    session_public_key: &'a PublicKey<33>,
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
    /// Session public key used for encrypted onion exit frames.
    pub session_public_key: PublicKey<33>,
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
        sign_descriptor_body(
            body,
            session_sk,
            "onion exit descriptor DID/public key/session mismatch",
        )
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            session_public_key,
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
            session_public_key,
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
            .any(|candidate| candidate.matches_route_service(service))
    }

    /// Return whether this descriptor offers `service` over `transport`.
    pub fn offers_service_transport(&self, service: &str, transport: OnionExitTransport) -> bool {
        self.services
            .iter()
            .any(|candidate| candidate.matches(service, transport))
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
                session_public_key: context.session_sk().session_public_key(),
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

#[cfg(test)]
mod tests;
