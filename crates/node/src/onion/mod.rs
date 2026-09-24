//! Application-layer circuit directory and route selection.
//!
//! This module deliberately sits in `rings-node`, not `rings-core`: Chord
//! remains the storage and discovery substrate, while exit policy is an
//! application protocol decision.
//!
//! The current data plane selects route-aware circuits and exit policies over layered
//! ElGamal-AEAD frames. A circuit is a pipeline (`pipeline`) over the static signature `Σ` of
//! operation symbols (`signature`): the pure reducer interprets the identity symbol `relay`, and
//! each node's Σ-algebra (`circuit::OnionAlgebra`) interprets the world-facing symbols it
//! registers. Route selection places a pipeline on a guard-closed loop (`loop_shape`) whose every
//! position is a node registering that position's symbol at its current process epoch.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
use rings_core::message::Decoder;
use rings_core::message::DomainTag;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
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
mod entry_guard;
pub(crate) mod exit_accounting;
mod failure;
#[cfg(rings_native)]
mod gateway;
#[cfg(any(rings_native, rings_browser))]
pub mod https;
mod loop_shape;
#[cfg(rings_native)]
pub mod native;
pub mod pipeline;
pub mod proxy;
pub(crate) mod replay;
mod role;
pub mod route;
pub mod signature;
pub mod target;
#[cfg(rings_native)]
pub mod tcp;

pub use entry_guard::OnionEntryGuardState;
pub use entry_guard::OnionEntryGuardStorage;
pub(crate) use entry_guard::OnionEntryGuards;
pub use failure::OnionExitFailure;
pub use failure::OnionRouteError;
#[cfg(rings_native)]
pub use gateway::NativeOnionGatewayConnector;
pub use loop_shape::OnionLoop;
pub use loop_shape::OnionLoopCursor;
pub use loop_shape::OnionLoopRelay;
pub use loop_shape::OnionLoopShape;
pub use loop_shape::OnionPending;
pub use loop_shape::OnionPipelineSymbols;
pub use loop_shape::MAX_ONION_LOOP_HOPS;
pub use loop_shape::MAX_ONION_LOOP_SYMBOLS;
pub use loop_shape::ONION_SEGMENT_RELAYS;
pub use role::OnionExitOffer;
pub use role::OnionRole;
pub(crate) use route::select_onion_route_from_candidates;
pub use route::OnionRoute;
pub(crate) use route::OnionRouteCandidates;
pub use route::OnionRouteHop;
pub use route::OnionRouteRequest;
pub(crate) use route::SystemRouteEntropy;
pub use signature::OnionServiceName;
pub use signature::OnionSymbolSpec;
pub use signature::ONION_SIGNATURE;
pub use target::OnionProxyTarget;
pub use target::OnionProxyTargetError;

/// DHT topic used for application-layer onion exit descriptors.
pub const ONION_EXITS_TOPIC: &str = "onion_exits";

/// Process epoch `e_n` of one node process (#834 D2), drawn afresh at every process start.
///
/// Every symbol a process registers carries its `e_n`: `relay` in the online-node capabilities and
/// every other symbol in its signed descriptor, so a process registers exactly one epoch,
///
/// ```text
/// ∀ f ∈ Σ_n.  epoch(f) = e_n,
/// ```
///
/// and a layer sealed for an earlier process of the same node names an epoch no current
/// registration carries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct OnionProcessEpoch([u8; 16]);

impl OnionProcessEpoch {
    /// Build a process epoch from explicit bytes.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Draw a fresh process epoch for one process lifetime.
    pub fn random() -> Self {
        Self(rand::random())
    }
}

const DEFAULT_ONION_EXIT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_ONION_EXIT_TTL_SECS: u64 = 90;
/// Message family of the onion-exit descriptor signature.
const ONION_EXIT_DESCRIPTOR_DOMAIN_TAG: DomainTag = domain_tag!("rings-node:onion-exit-descriptor");

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

/// Default native exit services: the world-facing symbols of [`ONION_SIGNATURE`] in table order.
/// It is only published when onion-exit advertisement is enabled.
pub fn default_onion_exit_services() -> Vec<OnionServiceName> {
    ONION_SIGNATURE.world_facing().collect()
}

/// Standard HTTPS onion-exit service set: the single `https` symbol of [`ONION_SIGNATURE`].
pub fn https_onion_exit_services() -> Vec<OnionServiceName> {
    vec![OnionServiceName::https()]
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
    const WILDCARD_AUTHORITY: &'static str = "*:*";

    /// Parse and canonicalize an exit target authority.
    pub fn parse(target: impl AsRef<str>) -> Result<Self> {
        let raw = target.as_ref().trim();
        if raw == "*" || raw == Self::WILDCARD_AUTHORITY {
            return Ok(Self(Self::WILDCARD_AUTHORITY.to_string()));
        }
        OnionProxyTarget::parse_authority(raw)
            .map(|target| Self(target.authority()))
            .map_err(|error| {
                Error::InvalidConfig(format!(
                    "invalid onion exit target {:?}; expected host:port or *:*: {error}",
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

    fn matches_target(&self, target: &Self) -> bool {
        self.0 == Self::WILDCARD_AUTHORITY || self == target
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
        self.allowed_targets
            .iter()
            .any(|allowed| allowed.matches_target(target))
    }

    fn denies(&self, target: &OnionExitTarget) -> bool {
        self.denied_targets
            .iter()
            .any(|denied| denied.matches_target(target))
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
    /// Delegation public key used for encrypted onion exit frames.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the registering process, equal to its `relay` registration's.
    pub process_epoch: OnionProcessEpoch,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Service this descriptor is willing to expose.
    pub service: OnionServiceName,
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
            delegatee_public_key: &self.delegatee_public_key,
            process_epoch: self.process_epoch,
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

    const DOMAIN_TAG: DomainTag = ONION_EXIT_DESCRIPTOR_DOMAIN_TAG;

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_network_id(&self) -> u32 {
        self.network_id
    }

    fn body_signing_data(&self) -> CoreResult<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        OnionExitDescriptor {
            did: self.did,
            public_key: self.public_key,
            delegatee_public_key: self.delegatee_public_key,
            process_epoch: self.process_epoch,
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
    did: Did,
    public_key: &'a VerificationPublicKey,
    delegatee_public_key: &'a PublicKey<33>,
    process_epoch: OnionProcessEpoch,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    service: &'a OnionServiceName,
    policy: &'a OnionExitPolicy,
    started_at_ms: u128,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    version: &'a str,
}

impl OnionExitDescriptorBodyRef<'_> {
    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        rings_codec::serialize(self).map_err(CoreError::CodecSerialize)
    }
}

/// Signed descriptor published by onion exits.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitDescriptor {
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Delegation public key used for encrypted onion exit frames.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the registering process, equal to its `relay` registration's.
    pub process_epoch: OnionProcessEpoch,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Service this descriptor is willing to expose.
    pub service: OnionServiceName,
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
    pub fn new_signed(
        body: OnionExitDescriptorBody,
        signer: MessageSigner<&DelegateeKey>,
    ) -> CoreResult<Self> {
        sign_descriptor_body(
            body,
            signer,
            "onion exit descriptor DID/public key/session mismatch",
        )
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            delegatee_public_key,
            process_epoch,
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
            did: *did,
            public_key,
            delegatee_public_key,
            process_epoch: *process_epoch,
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

    /// Return whether this descriptor advertises the requested service name.
    pub fn advertises_service_name(&self, service: &str) -> bool {
        self.service.matches(service)
    }

    /// Return whether this descriptor offers `service`.
    pub fn offers_service(&self, service: &str) -> bool {
        self.service.matches(service)
    }

    /// Verify the descriptor signature and DID/public-key binding.
    pub fn verify_signature(&self, network_id: u32) -> bool {
        self.descriptor_verify_signature(network_id)
    }

    /// Returns whether this descriptor is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_expired_at(now_ms)
    }

    /// Returns whether this descriptor has a valid signature and is not expired.
    pub fn is_live_at(&self, now_ms: u128, network_id: u32) -> bool {
        self.verify_signature(network_id) && !self.is_expired_at(now_ms)
    }

    /// Select the newest valid onion-exit descriptor per `(DID, service)`.
    ///
    /// Invariant: an exit may publish independent TCP and HTTPS registrations under the same DID.
    /// Preservation: heartbeat ordering is compared only inside each `(DID, service name)` key.
    pub fn latest_valid_by_service_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        let mut latest = BTreeMap::<(Did, OnionServiceName), Self>::new();
        for descriptor in descriptors {
            if include_expired {
                if !descriptor.verify_signature(network_id) {
                    continue;
                }
            } else if !descriptor.is_live_at(now_ms, network_id) {
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
    type Body = OnionExitDescriptorBody;

    fn descriptor_did(&self) -> Did {
        self.did
    }

    fn descriptor_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn descriptor_network_id(&self) -> u32 {
        self.network_id
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

/// Result of decoding one onion-exit registry entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnionExitDescriptorDecodeReport {
    /// Descriptors that decoded successfully under the total-cutover wire shape.
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
pub(crate) struct OnionExitRegistration {
    heartbeat_interval: Duration,
    ttl: Duration,
    node_type: OnlineNodeType,
    process_epoch: OnionProcessEpoch,
    started_at_ms: u128,
    offer: OnionExitOffer,
    publisher: DhtRegistrationPublisher,
}

impl OnionExitRegistration {
    pub(crate) fn new(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        offer: OnionExitOffer,
        process_epoch: OnionProcessEpoch,
    ) -> Self {
        Self {
            heartbeat_interval,
            ttl,
            node_type,
            process_epoch,
            started_at_ms: get_epoch_ms(),
            offer,
            publisher: DhtRegistrationPublisher::new(ONION_EXITS_TOPIC),
        }
    }

    /// Build this node's signed onion-exit descriptors at `now_ms`.
    pub fn descriptors_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
    ) -> Result<Vec<OnionExitDescriptor>> {
        self.offer
            .services()
            .iter()
            .cloned()
            .map(|service| self.descriptor_for_service(context, now_ms, service))
            .collect()
    }

    fn descriptor_for_service(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
        service: OnionServiceName,
    ) -> Result<OnionExitDescriptor> {
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: context.did(),
                public_key: context.delegator_verification_pubkey()?,
                delegatee_public_key: context.delegatee_key().delegatee_public_key(),
                process_epoch: self.process_epoch,
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                service,
                policy: self.offer.policy().clone(),
                started_at_ms: self.started_at_ms,
                heartbeat_at_ms: now_ms,
                expires_at_ms: now_ms + self.ttl.as_millis(),
                version: crate::util::build_version(),
            },
            context.message_signer(),
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
        self.publisher
            .publish_replacing(context, encoded, |observed| {
                observed
                    .decode::<OnionExitDescriptor>()
                    .is_ok_and(|descriptor| {
                        descriptor.did == context.did()
                            || (descriptor.verify_signature(context.network_id())
                                && descriptor.is_expired_at(now_ms))
                    })
            })
            .await?;
        Ok(descriptors)
    }

    /// Decode onion-exit descriptors from a DHT entry.
    pub fn decode_descriptors_from_entry(
        entry: &rings_core::dht::entry::Entry,
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
        entry: &rings_core::dht::entry::Entry,
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

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
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
