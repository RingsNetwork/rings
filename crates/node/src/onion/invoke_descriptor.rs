//! Signed descriptors and DHT registration for intermediate `invoke` symbols (#834 D1′, D2).
//!
//! A node publishes one signed [`OnionInvokeDescriptor`] per `(shape, backend)` pair in `Σ_I × Ω`
//! under [`ONION_INVOKES_TOPIC`], carrying the same `process_epoch` `e_n` as its `relay`
//! registration and the `quota(n, f, ω)` policy ([`OnionInvokePolicy`]). Applications and client
//! constants are never published.

use std::borrow::Borrow;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_core::utils::SharedClock;
use serde::Deserialize;
use serde::Serialize;

use super::role::OnionInvokeOffer;
use super::signature::OnionBackendName;
use super::signature::OnionInvokeShape;
use super::OnionProcessEpoch;
use super::OnionProcessEpochCell;
use crate::descriptor::heard_elements;
use crate::descriptor::latest_valid_by_key;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeType;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::LeaseTerms;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

/// DHT topic used for signed intermediate `invoke` symbol descriptors (#834 D2).
pub const ONION_INVOKES_TOPIC: &str = "onion_invokes";

/// Message family of the onion-invoke descriptor signature.
const ONION_INVOKE_DESCRIPTOR_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-node:onion-invoke-descriptor");

/// Signed `quota(n, f, ω)` policy for one `(node, shape, backend)` registration (#834 D2).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OnionInvokePolicy {
    /// Maximum concurrent in-flight invocations of `(shape, backend)` on this node.
    /// `0` means unspecified (clamped to the protocol hard bound).
    pub max_concurrent: u32,
    /// Maximum invocations per minute of `(shape, backend)` on this node.
    /// `0` means unspecified.
    pub max_invocations_per_minute: u64,
}

/// Descriptor fields covered by the onion-invoke signature.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OnionInvokeDescriptorBody {
    /// DID of the hosting node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Delegation public key: the Sphinx key `pk_i` a loop's header blinds the hop's layer to.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the registering process, equal to its `relay` registration's.
    pub process_epoch: OnionProcessEpoch,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Closed `invoke` shape `f ∈ Σ_I`.
    pub shape: OnionInvokeShape,
    /// Canonical open backend name `ω ∈ Ω`.
    pub backend: OnionBackendName,
    /// Signed per-`(node, shape, backend)` quota policy.
    pub policy: OnionInvokePolicy,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// Freshness window `T` in milliseconds.
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
}

impl OnionInvokeDescriptorBody {
    fn body_ref(&self) -> OnionInvokeDescriptorBodyRef<'_> {
        OnionInvokeDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            delegatee_public_key: &self.delegatee_public_key,
            process_epoch: self.process_epoch,
            node_type: &self.node_type,
            network_id: self.network_id,
            shape: &self.shape,
            backend: &self.backend,
            policy: &self.policy,
            started_at_ms: self.started_at_ms,
            fresh_for_ms: self.fresh_for_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version.as_str(),
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        self.body_ref().signing_data()
    }
}

impl SignedDescriptorBody for OnionInvokeDescriptorBody {
    type Descriptor = OnionInvokeDescriptor;

    const DOMAIN_TAG: DomainTag = ONION_INVOKE_DESCRIPTOR_DOMAIN_TAG;

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
        OnionInvokeDescriptor {
            did: self.did,
            public_key: self.public_key,
            delegatee_public_key: self.delegatee_public_key,
            process_epoch: self.process_epoch,
            node_type: self.node_type,
            network_id: self.network_id,
            shape: self.shape,
            backend: self.backend,
            policy: self.policy,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: 0,
            fresh_for_ms: self.fresh_for_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version,
            signature,
        }
    }
}

#[derive(Serialize)]
struct OnionInvokeDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    delegatee_public_key: &'a PublicKey<33>,
    process_epoch: OnionProcessEpoch,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    shape: &'a OnionInvokeShape,
    backend: &'a OnionBackendName,
    policy: &'a OnionInvokePolicy,
    started_at_ms: u128,
    fresh_for_ms: u64,
    expires_at_ms: u128,
    version: &'a str,
}

impl OnionInvokeDescriptorBodyRef<'_> {
    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        rings_codec::serialize(self).map_err(CoreError::CodecSerialize)
    }
}

/// Signed descriptor published for one `(shape, backend)` `invoke` registration (#834 D2).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OnionInvokeDescriptor {
    /// DID of the hosting node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Delegation public key: the Sphinx key `pk_i` a loop's header blinds the hop's layer to.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the registering process, equal to its `relay` registration's.
    pub process_epoch: OnionProcessEpoch,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Closed `invoke` shape `f ∈ Σ_I`.
    pub shape: OnionInvokeShape,
    /// Canonical open backend name `ω ∈ Ω`.
    pub backend: OnionBackendName,
    /// Signed per-`(node, shape, backend)` quota policy.
    pub policy: OnionInvokePolicy,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// When the reader last heard this descriptor, in milliseconds since Unix epoch (`0` until heard).
    #[serde(skip)]
    pub heartbeat_at_ms: u128,
    /// Freshness window `T` in milliseconds.
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
    /// Signature covering every field above except `heartbeat_at_ms`.
    pub signature: MessageVerification,
}

impl OnionInvokeDescriptor {
    /// Create and sign an onion-invoke descriptor.
    pub fn new_signed(
        body: OnionInvokeDescriptorBody,
        signer: MessageSigner<impl Borrow<DelegateeKey>>,
    ) -> CoreResult<Self> {
        sign_descriptor_body(
            body,
            signer,
            "onion invoke descriptor DID/public key/session mismatch",
        )
    }

    fn body_ref(&self) -> OnionInvokeDescriptorBodyRef<'_> {
        OnionInvokeDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            delegatee_public_key: &self.delegatee_public_key,
            process_epoch: self.process_epoch,
            node_type: &self.node_type,
            network_id: self.network_id,
            shape: &self.shape,
            backend: &self.backend,
            policy: &self.policy,
            started_at_ms: self.started_at_ms,
            fresh_for_ms: self.fresh_for_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version.as_str(),
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        self.body_ref().signing_data()
    }

    /// Verify the descriptor signature and DID/public-key binding.
    pub fn verify_signature(&self, network_id: u32) -> bool {
        self.descriptor_verify_signature(network_id)
    }

    /// Returns whether this descriptor is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_expired_at(now_ms)
    }

    /// Returns whether a reader lists this descriptor as live at `now_ms`.
    pub fn is_live_at(&self, now_ms: u128, network_id: u32) -> bool {
        self.descriptor_is_live_at(now_ms, network_id)
    }

    /// Select the newest valid onion-invoke descriptor per `(DID, shape, backend)` (#834 D2).
    pub fn latest_valid_by_symbol_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_key(
            descriptors,
            Self::registration_key,
            now_ms,
            network_id,
            include_expired,
        )
        .into_values()
        .collect()
    }

    /// Return the registration key `(DID, shape, backend)`.
    pub(crate) fn registration_key(&self) -> (Did, OnionInvokeShape, OnionBackendName) {
        (self.did, self.shape, self.backend.clone())
    }
}

impl SignedDescriptor for OnionInvokeDescriptor {
    type Body = OnionInvokeDescriptorBody;

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

    fn descriptor_fresh_for_ms(&self) -> u64 {
        self.fresh_for_ms
    }

    fn descriptor_signing_data(&self) -> CoreResult<Vec<u8>> {
        self.signing_data()
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        Self {
            heartbeat_at_ms,
            ..self
        }
    }
}

impl RegistryElement for OnionInvokeDescriptor {}

/// Result of decoding one onion-invoke registry entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OnionInvokeDescriptorDecodeReport {
    /// Descriptors that decoded successfully under the closed signature and backend grammar.
    pub descriptors: Vec<OnionInvokeDescriptor>,
    /// Number of registry values explicitly rejected at the decode boundary.
    pub rejected_values: usize,
}

impl OnionInvokeDescriptorDecodeReport {
    /// Return whether the entry contained values this node could not decode.
    pub const fn has_rejections(&self) -> bool {
        self.rejected_values > 0
    }
}

/// Periodic node-layer DHT registration for offered `(shape, backend)` pairs.
#[derive(Clone, Debug)]
pub(crate) struct OnionInvokeRegistration {
    heartbeat_interval: Duration,
    node_type: OnlineNodeType,
    process_epoch: OnionProcessEpochCell,
    started_at_ms: u128,
    offer: OnionInvokeOffer,
    clock: SharedClock,
    publisher: DhtRegistrationPublisher<Vec<OnionInvokeDescriptor>>,
}

impl OnionInvokeRegistration {
    pub(crate) fn new_with_clock(
        heartbeat_interval: Duration,
        ttl: Duration,
        node_type: OnlineNodeType,
        offer: OnionInvokeOffer,
        process_epoch: OnionProcessEpochCell,
        clock: SharedClock,
    ) -> Self {
        let started_at_ms = clock.now_ms();
        Self {
            heartbeat_interval,
            node_type,
            process_epoch,
            started_at_ms,
            offer,
            clock,
            publisher: DhtRegistrationPublisher::new(ONION_INVOKES_TOPIC, heartbeat_interval, ttl),
        }
    }

    /// Build this node's signed `invoke` descriptors under `terms`.
    pub fn descriptors_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<Vec<OnionInvokeDescriptor>> {
        self.offer
            .entries()
            .map(|((shape, backend), policy)| {
                OnionInvokeDescriptor::new_signed(
                    OnionInvokeDescriptorBody {
                        did: context.did(),
                        public_key: context.delegator_verification_pubkey()?,
                        delegatee_public_key: context.delegatee_key().delegatee_public_key(),
                        process_epoch: self.process_epoch.get(),
                        node_type: self.node_type.clone(),
                        network_id: context.network_id(),
                        shape: *shape,
                        backend: backend.clone(),
                        policy: *policy,
                        started_at_ms: self.started_at_ms,
                        fresh_for_ms: terms.fresh_for_ms,
                        expires_at_ms: terms.expires_at_ms,
                        version: crate::util::build_version(),
                    },
                    context.message_signer(),
                )
                .map_err(Error::CoreError)
            })
            .collect()
    }

    /// Publish one heartbeat of this node's signed `invoke` descriptors.
    pub async fn publish_descriptors(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<Vec<OnionInvokeDescriptor>> {
        self.publisher
            .publish_lease(context, self.clock.now_ms(), |terms| {
                self.descriptors_at(context, terms)
            })
            .await
    }

    /// Decode `invoke` descriptors from a DHT entry, counting schema rejections.
    pub fn decode_descriptors_from_entry(
        entry: &rings_core::dht::entry::Entry,
    ) -> OnionInvokeDescriptorDecodeReport {
        let mut report = OnionInvokeDescriptorDecodeReport::default();
        for (value, heard_at_ms) in heard_elements(entry) {
            match OnionInvokeDescriptor::from_element(value) {
                Ok(descriptor) => report.descriptors.push(descriptor.heard_at(heard_at_ms)),
                Err(error) => {
                    report.rejected_values = report.rejected_values.saturating_add(1);
                    tracing::debug!(
                        "rejected onion-invoke descriptor registry value at schema boundary: {error}"
                    );
                }
            }
        }
        report
    }

    /// Decode `invoke` descriptors from a DHT entry, dropping rejected values.
    pub fn descriptors_from_entry(
        entry: &rings_core::dht::entry::Entry,
    ) -> Vec<OnionInvokeDescriptor> {
        let report = Self::decode_descriptors_from_entry(entry);
        if report.has_rejections() {
            tracing::warn!(
                rejected_values = report.rejected_values,
                "ignored unsupported onion-invoke descriptor registry values"
            );
        }
        report.descriptors
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl RegistrationTask for OnionInvokeRegistration {
    fn name(&self) -> &'static str {
        "onion-invoke"
    }

    fn interval(&self) -> Duration {
        self.heartbeat_interval
    }

    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
        self.publish_descriptors(context).await.map(|_| ())
    }
}
