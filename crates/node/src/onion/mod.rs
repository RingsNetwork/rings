#![warn(missing_docs)]
//! Application-layer circuit directory and route selection.
//!
//! This module deliberately sits in `rings-node`, not `rings-core`: Chord
//! remains the storage and discovery substrate, while exit policy is an
//! application protocol decision.
//!
//! The current data plane selects route-aware circuits and exit policies, with layered
//! ElGamal-AEAD frames described by [`circuit::ONION_CIRCUIT_SECURITY`].

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
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
pub(crate) mod directory;
pub(crate) mod exit_accounting;
mod failure;
#[cfg(feature = "browser")]
pub mod https;
pub mod proxy;
pub(crate) mod replay;
pub mod route;
pub mod target;
#[cfg(feature = "node")]
pub mod tcp;

pub use failure::OnionExitFailure;
pub use failure::OnionRouteError;
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

const ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION: u16 = 2;

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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
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
#[derive(Clone, Debug, Deserialize, Serialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OnionExitService {
    /// Service name advertised to route builders.
    pub name: OnionServiceName,
    /// Transport used by this service.
    pub transport: OnionExitTransport,
}

/// Canonical onion-exit service name.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionServiceName(String);

impl OnionServiceName {
    /// Parse and canonicalize a service name.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed != name {
            return Err(Error::InvalidConfig(
                "onion exit service name must be non-empty and trimmed".to_string(),
            ));
        }
        if trimmed.len() > 64 || trimmed.chars().any(|ch| !is_service_name_char(ch)) {
            return Err(Error::InvalidConfig(format!(
                "invalid onion exit service name {name:?}; expected [A-Za-z0-9._-] up to 64 bytes"
            )));
        }
        Ok(Self(trimmed.to_ascii_lowercase()))
    }

    /// Return the standard browser HTTPS exit service name.
    pub fn https() -> Self {
        Self::static_name("https")
    }

    /// Return the standard native TCP exit service name.
    pub fn tcp() -> Self {
        Self::static_name("tcp")
    }

    /// Build a trusted static service name.
    fn static_name(name: &'static str) -> Self {
        Self(name.to_string())
    }

    /// Return the service name as a string slice.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Return whether this name equals `service` after service-name canonicalization.
    pub fn matches(&self, service: &str) -> bool {
        Self::parse(service).is_ok_and(|candidate| candidate == *self)
    }
}

impl TryFrom<String> for OnionServiceName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionServiceName> for String {
    fn from(name: OnionServiceName) -> Self {
        name.0
    }
}

impl OnionExitService {
    /// Return a named exit service with an explicit transport.
    pub fn new(name: impl AsRef<str>, transport: OnionExitTransport) -> Result<Self> {
        Ok(Self::from_name(OnionServiceName::parse(name)?, transport))
    }

    /// Return a named exit service from an already validated name.
    pub fn from_name(name: OnionServiceName, transport: OnionExitTransport) -> Self {
        Self { name, transport }
    }

    /// Return the standard browser HTTPS exit service.
    pub fn https() -> Self {
        Self::from_name(OnionServiceName::https(), OnionExitTransport::Https)
    }

    /// Return the standard native TCP exit service.
    pub fn tcp() -> Self {
        Self::from_name(OnionServiceName::tcp(), OnionExitTransport::Tcp)
    }

    /// Return whether this service has the requested name.
    pub fn has_name(&self, service: &str) -> bool {
        self.name.matches(service)
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
        let service = OnionServiceName::parse(service).ok()?;
        match service.as_str() {
            "tcp" => Some(OnionExitTransport::Tcp),
            "https" => Some(OnionExitTransport::Https),
            _ => None,
        }
    }
}

fn is_service_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// Signed policy fields for an onion exit.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitPolicy {
    /// Target allow-list entries understood by the exit implementation. Empty means closed.
    pub allowed_targets: Vec<OnionExitTarget>,
    /// Target deny-list entries understood by the exit implementation. Deny entries override allows.
    pub denied_targets: Vec<OnionExitTarget>,
    /// Maximum concurrent circuits this exit wants to serve. `0` means unspecified.
    pub max_circuits: u32,
    /// Maximum streams per circuit. `0` means unspecified.
    pub max_streams_per_circuit: u32,
    /// Maximum bytes per minute. `0` means unspecified.
    pub max_bytes_per_minute: u64,
}

/// Canonical target authority admitted by an onion exit policy.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OnionExitTarget(String);

impl OnionExitTarget {
    /// Parse and canonicalize an exit target authority.
    pub fn parse(target: impl AsRef<str>) -> Result<Self> {
        OnionProxyTarget::parse_authority(target.as_ref())
            .map(|target| Self(target.authority()))
            .map_err(|error| {
                Error::InvalidConfig(format!(
                    "invalid onion exit target {:?}; expected host:port: {error}",
                    target.as_ref()
                ))
            })
    }

    /// Return the canonical host:port authority.
    pub fn authority(&self) -> &str {
        self.0.as_str()
    }

    /// Build a policy target from an already-validated proxy target.
    pub fn from_proxy_target(target: &OnionProxyTarget) -> Self {
        Self(target.authority())
    }
}

impl TryFrom<String> for OnionExitTarget {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<OnionExitTarget> for String {
    fn from(target: OnionExitTarget) -> Self {
        target.0
    }
}

impl OnionExitPolicy {
    /// Build a policy from raw target strings at configuration or API boundaries.
    pub fn from_target_strings(
        allowed_targets: Vec<String>,
        denied_targets: Vec<String>,
    ) -> Result<Self> {
        Ok(Self {
            allowed_targets: parse_exit_targets(allowed_targets)?,
            denied_targets: parse_exit_targets(denied_targets)?,
            ..Self::default()
        })
    }

    /// Return whether this policy denies every exit target.
    pub fn is_closed(&self) -> bool {
        self.allowed_targets.is_empty()
    }

    /// Validate target lists for an advertised onion exit.
    pub fn validate_targets(&self) -> Result<()> {
        if self.is_closed() {
            return Err(Error::InvalidConfig(
                "advertise_onion_exit requires at least one valid onion_exit_policy allowed target"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Return whether `target` is admitted by this policy's allow-list.
    pub fn allows_target(&self, target: &OnionExitTarget) -> bool {
        if self.is_closed() {
            return false;
        }
        if self.denies(target) {
            return false;
        }
        self.allows(target)
    }

    fn allows(&self, target: &OnionExitTarget) -> bool {
        self.allowed_targets.iter().any(|allowed| allowed == target)
    }

    fn denies(&self, target: &OnionExitTarget) -> bool {
        self.denied_targets.iter().any(|denied| denied == target)
    }
}

fn parse_exit_targets(targets: Vec<String>) -> Result<Vec<OnionExitTarget>> {
    targets
        .into_iter()
        .map(OnionExitTarget::parse)
        .collect::<Result<Vec<_>>>()
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
    /// Service this descriptor is willing to expose.
    pub service: OnionExitService,
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
            schema_version: ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION,
            did: self.did,
            public_key: &self.public_key,
            session_public_key: &self.session_public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            service: &self.service,
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
            schema_version: ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION,
            did: self.did,
            public_key: self.public_key,
            session_public_key: self.session_public_key,
            node_type: self.node_type,
            network_id: self.network_id,
            service: self.service,
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
    schema_version: u16,
    did: Did,
    public_key: &'a VerificationPublicKey,
    session_public_key: &'a PublicKey<33>,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    service: &'a OnionExitService,
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
    /// Onion-exit descriptor wire schema version covered by the descriptor signature.
    pub schema_version: u16,
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
    /// Service this descriptor is willing to expose.
    pub service: OnionExitService,
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
            schema_version,
            did,
            public_key,
            session_public_key,
            node_type,
            network_id,
            service,
            policy,
            started_at_ms,
            heartbeat_at_ms,
            expires_at_ms,
            version,
            signature: _,
        } = self;

        OnionExitDescriptorBodyRef {
            schema_version: *schema_version,
            did: *did,
            public_key,
            session_public_key,
            node_type,
            network_id: *network_id,
            service,
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

    /// Return whether this descriptor uses the local registry wire schema.
    pub const fn has_supported_schema(&self) -> bool {
        self.schema_version == ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION
    }

    /// Return whether this descriptor belongs to `network_id`.
    pub const fn matches_network(&self, network_id: u32) -> bool {
        self.network_id == network_id
    }

    /// Return whether this descriptor advertises the requested service name.
    pub fn advertises_service_name(&self, service: &str) -> bool {
        self.service.has_name(service)
    }

    /// Return whether this descriptor offers `service`.
    pub fn offers_service(&self, service: &str) -> bool {
        self.service.matches_route_service(service)
    }

    /// Return whether this descriptor offers `service` over `transport`.
    pub fn offers_service_transport(&self, service: &str, transport: OnionExitTransport) -> bool {
        self.service.matches(service, transport)
    }

    /// Verify the descriptor signature and DID/public-key binding.
    pub fn verify_signature(&self) -> bool {
        self.has_supported_schema() && self.descriptor_verify_signature()
    }

    /// Returns whether this descriptor is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_expired_at(now_ms)
    }

    /// Returns whether this descriptor has a valid signature and is not expired.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.verify_signature() && !self.is_expired_at(now_ms)
    }

    /// Select the newest valid onion-exit descriptor per `(DID, service)`.
    ///
    /// Invariant: an exit may publish independent TCP and HTTPS registrations under the same DID.
    /// Preservation: heartbeat ordering is compared only inside each `(DID, service name,
    /// transport)` key.
    pub fn latest_valid_by_service_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        include_expired: bool,
    ) -> Vec<Self> {
        let mut latest = BTreeMap::<(Did, OnionExitService), Self>::new();
        for descriptor in descriptors {
            if include_expired {
                if !descriptor.verify_signature() {
                    continue;
                }
            } else if !descriptor.is_live_at(now_ms) {
                continue;
            }
            let key = (descriptor.did, descriptor.service.clone());
            match latest.entry(key) {
                Entry::Occupied(mut entry) => {
                    if descriptor.heartbeat_at_ms > entry.get().heartbeat_at_ms {
                        entry.insert(descriptor);
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(descriptor);
                }
            }
        }
        latest.into_values().collect()
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
        let descriptor: Self = decode_descriptor(encoded)?;
        if descriptor.has_supported_schema() {
            Ok(descriptor)
        } else {
            Err(CoreError::Decode)
        }
    }
}

/// Result of decoding one onion-exit registry entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnionExitDescriptorDecodeReport {
    /// Descriptors that matched the local wire schema and decoded successfully.
    pub descriptors: Vec<OnionExitDescriptor>,
    /// Number of registry values explicitly rejected at the decode boundary.
    pub rejected_values: usize,
}

impl OnionExitDescriptorDecodeReport {
    /// Return whether the entry contained values this node could not decode.
    pub const fn has_rejections(&self) -> bool {
        self.rejected_values > 0
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

    /// Build this node's signed onion-exit descriptors at `now_ms`.
    pub fn descriptors_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
    ) -> Result<Vec<OnionExitDescriptor>> {
        self.services
            .iter()
            .cloned()
            .map(|service| self.descriptor_for_service(context, now_ms, service))
            .collect()
    }

    fn descriptor_for_service(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
        service: OnionExitService,
    ) -> Result<OnionExitDescriptor> {
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: context.did(),
                public_key: context.account_verification_pubkey()?,
                session_public_key: context.session_sk().session_public_key(),
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                service,
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

    /// Publish this node's signed onion-exit descriptors.
    pub async fn publish_descriptors(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<Vec<OnionExitDescriptor>> {
        let now_ms = get_epoch_ms();
        let descriptors = self.descriptors_at(context, now_ms)?;
        let encoded = descriptors
            .iter()
            .map(|descriptor| descriptor.encode().map_err(Error::CoreError))
            .collect::<Result<Vec<_>>>()?;
        self.publisher.publish_many(context, encoded).await?;
        Ok(descriptors)
    }

    /// Decode onion-exit descriptors from a DHT entry.
    pub fn decode_descriptors_from_entry(
        entry: &rings_core::prelude::entry::Entry,
    ) -> OnionExitDescriptorDecodeReport {
        let mut report = OnionExitDescriptorDecodeReport::default();
        for value in &entry.data {
            match value.decode::<OnionExitDescriptor>() {
                Ok(descriptor) => report.descriptors.push(descriptor),
                Err(error) => {
                    report.rejected_values = report.rejected_values.saturating_add(1);
                    tracing::debug!(
                        "rejected onion-exit descriptor registry value at schema boundary: {error}"
                    );
                }
            }
        }
        report
    }

    /// Decode onion-exit descriptors from a DHT entry, dropping values rejected at the schema boundary.
    pub fn descriptors_from_entry(
        entry: &rings_core::prelude::entry::Entry,
    ) -> Vec<OnionExitDescriptor> {
        let report = Self::decode_descriptors_from_entry(entry);
        if report.has_rejections() {
            tracing::warn!(
                rejected_values = report.rejected_values,
                "ignored unsupported onion-exit descriptor registry values"
            );
        }
        report.descriptors
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
        self.publish_descriptors(context).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests;
