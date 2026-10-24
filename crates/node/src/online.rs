//! Signed online-node descriptors stored in the DHT.

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::dht::RelayCustody;
use rings_core::domain_tag;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::DhtProtocolMode;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::latest_valid_by_did;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::onion::OnionProcessEpoch;

/// DHT topic used for online-node registry descriptors.
pub const ONLINE_NODES_TOPIC: &str = "online_nodes";
/// Message family of the online-node descriptor signature.
const ONLINE_NODE_DESCRIPTOR_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-node:online-node-descriptor");

/// Runtime family advertised by a node descriptor.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum OnlineNodeType {
    /// Browser runtime.
    Browser,
    /// Native node runtime.
    Native,
    /// FFI runtime.
    Ffi,
}

/// Typed capabilities of one node process, published in its online-node descriptor.
///
/// `onion_relay = Some(e_n)` registers the identity symbol `relay` of the onion signature at the
/// process epoch `e_n` (#834 D2). The epoch is part of the capability, so a relay registration
/// without an epoch is unrepresentable.
///
/// `relay_custody` is the declaration the node also makes in every connection handshake: a node
/// without a retained relay store holds no relay mail, and relay custody moves past it to the
/// next node along the successor path that holds relay mail (`rings_core::dht::relay_holder`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnlineNodeCapabilities {
    /// Process epoch at which this node registers the onion `relay` symbol, if it does.
    pub onion_relay: Option<OnionProcessEpoch>,
    /// Whether this node holds relay mail.
    pub relay_custody: RelayCustody,
}

impl OnlineNodeCapabilities {
    /// Register the onion `relay` symbol at process epoch `epoch`, holding relay mail.
    pub const fn onion_relay(epoch: OnionProcessEpoch) -> Self {
        Self {
            onion_relay: Some(epoch),
            relay_custody: RelayCustody::Holds,
        }
    }
}

/// Descriptor fields covered by the online-node signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnlineNodeDescriptorBody {
    /// DID of the node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Delegation public key used for encrypted onion relay frames.
    pub delegatee_public_key: PublicKey<33>,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
    /// Typed capabilities of this node process.
    pub capabilities: OnlineNodeCapabilities,
    /// Optional endpoint hint, controlled by node policy/configuration.
    pub endpoint_hint: Option<String>,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// Freshness window `T` in milliseconds: a reader holds the descriptor live only while it
    /// heard it within `T` (see the lease law of `descriptor`).
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
}

impl OnlineNodeDescriptorBody {
    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        OnlineNodeDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            delegatee_public_key: &self.delegatee_public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
            capabilities: self.capabilities,
            endpoint_hint: &self.endpoint_hint,
            started_at_ms: self.started_at_ms,
            fresh_for_ms: self.fresh_for_ms,
            expires_at_ms: self.expires_at_ms,
            version: self.version.as_str(),
        }
    }

    fn signing_data(&self) -> Result<Vec<u8>> {
        self.body_ref().signing_data()
    }
}

impl SignedDescriptorBody for OnlineNodeDescriptorBody {
    type Descriptor = OnlineNodeDescriptor;

    const DOMAIN_TAG: DomainTag = ONLINE_NODE_DESCRIPTOR_DOMAIN_TAG;

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_network_id(&self) -> u32 {
        self.network_id
    }

    fn body_signing_data(&self) -> Result<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        OnlineNodeDescriptor {
            did: self.did,
            public_key: self.public_key,
            delegatee_public_key: self.delegatee_public_key,
            node_type: self.node_type,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
            capabilities: self.capabilities,
            endpoint_hint: self.endpoint_hint,
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
struct OnlineNodeDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    delegatee_public_key: &'a PublicKey<33>,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    capabilities: OnlineNodeCapabilities,
    endpoint_hint: &'a Option<String>,
    started_at_ms: u128,
    fresh_for_ms: u64,
    expires_at_ms: u128,
    version: &'a str,
}

impl OnlineNodeDescriptorBodyRef<'_> {
    fn signing_data(&self) -> Result<Vec<u8>> {
        rings_codec::serialize(self).map_err(Error::CodecSerialize)
    }
}

/// Signed descriptor published by online nodes.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnlineNodeDescriptor {
    /// DID of the node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Delegation public key used for encrypted onion relay frames.
    pub delegatee_public_key: PublicKey<33>,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
    /// Typed capabilities of this node process.
    pub capabilities: OnlineNodeCapabilities,
    /// Optional endpoint hint, controlled by node policy/configuration.
    pub endpoint_hint: Option<String>,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// When the reader last heard this descriptor, in milliseconds since Unix epoch: the dot of
    /// its element in the fetched registry carrier. Not signed and not encoded (see the lease
    /// law of `descriptor`); `0` until heard.
    #[serde(skip)]
    pub heartbeat_at_ms: u128,
    /// Freshness window `T` in milliseconds.
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
    /// Signature covering every descriptor field above except `heartbeat_at_ms`.
    pub signature: MessageVerification,
}

impl OnlineNodeDescriptor {
    /// Create and sign a descriptor.
    pub fn new_signed(
        body: OnlineNodeDescriptorBody,
        signer: MessageSigner<&DelegateeKey>,
    ) -> Result<Self> {
        sign_descriptor_body(
            body,
            signer,
            "online node descriptor DID/public key/session mismatch",
        )
    }

    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            delegatee_public_key,
            node_type,
            network_id,
            storage_redundancy,
            dht_virtual_nodes,
            capabilities,
            endpoint_hint,
            started_at_ms,
            heartbeat_at_ms: _,
            fresh_for_ms,
            expires_at_ms,
            version,
            signature: _,
        } = self;

        OnlineNodeDescriptorBodyRef {
            did: *did,
            public_key,
            delegatee_public_key,
            node_type,
            network_id: *network_id,
            storage_redundancy: *storage_redundancy,
            dht_virtual_nodes: *dht_virtual_nodes,
            capabilities: *capabilities,
            endpoint_hint,
            started_at_ms: *started_at_ms,
            fresh_for_ms: *fresh_for_ms,
            expires_at_ms: *expires_at_ms,
            version: version.as_str(),
        }
    }

    fn signing_data(&self) -> Result<Vec<u8>> {
        self.body_ref().signing_data()
    }

    /// Return this descriptor's DHT protocol mode.
    pub const fn dht_protocol_mode(&self) -> DhtProtocolMode {
        DhtProtocolMode::new(
            self.network_id,
            self.storage_redundancy,
            self.dht_virtual_nodes,
        )
    }

    /// Return whether this descriptor belongs to the local DHT protocol mode.
    pub fn matches_dht_protocol(&self, expected: DhtProtocolMode) -> bool {
        self.dht_protocol_mode().matches(expected)
    }

    /// Verify the descriptor signature under the receiver's overlay `network_id` and the
    /// DID/public-key binding.
    ///
    /// This does not apply the embedded [`MessageVerification`] timestamp/TTL
    /// as a liveness rule. Online-node liveness is defined by the lease law of
    /// `descriptor`: the signed lease end and freshness window, and the time the
    /// descriptor was heard; use [`Self::is_live_at`] when it should be enforced.
    pub fn verify_signature(&self, network_id: u32) -> bool {
        self.descriptor_verify_signature(network_id)
    }

    /// Returns whether this descriptor is expired at `now_ms`: its lease ended, or it was not
    /// heard within its freshness window.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.descriptor_is_expired_at(now_ms)
    }

    /// Returns whether a reader lists this descriptor: it verifies under the overlay
    /// `network_id`, its lease terms are admissible, and it is not expired (the lease law of
    /// `descriptor`).
    pub fn is_live_at(&self, now_ms: u128, network_id: u32) -> bool {
        self.descriptor_is_live_at(now_ms, network_id)
    }

    /// Select the newest descriptor per DID that verifies under the overlay `network_id`.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_did(descriptors, now_ms, network_id, include_expired)
    }
}

impl SignedDescriptor for OnlineNodeDescriptor {
    type Body = OnlineNodeDescriptorBody;

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

    fn descriptor_signing_data(&self) -> Result<Vec<u8>> {
        self.signing_data()
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        Self {
            heartbeat_at_ms,
            ..self
        }
    }
}

impl RegistryElement for OnlineNodeDescriptor {}

#[cfg(test)]
mod tests {
    use rings_core::consts::TS_OFFSET_TOLERANCE_MS;
    use rings_core::delegation::DelegateeKey;
    use rings_core::ecc::SecretKey;

    use super::*;
    use crate::tests::TEST_NETWORK_ID;

    /// A descriptor of a fresh account, fresh for `fresh_for_ms` until the lease end
    /// `expires_at_ms`, heard at `heartbeat_at_ms`.
    fn descriptor_at(
        heartbeat_at_ms: u128,
        fresh_for_ms: u64,
        expires_at_ms: u128,
    ) -> Result<OnlineNodeDescriptor> {
        let key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
        Ok(
            descriptor_of(&delegatee_key, "test", fresh_for_ms, expires_at_ms)?
                .heard_at(heartbeat_at_ms),
        )
    }

    /// A descriptor of `delegatee_key`'s node at `version`, fresh for `fresh_for_ms` until the
    /// lease end `expires_at_ms`, not yet heard.
    fn descriptor_of(
        delegatee_key: &DelegateeKey,
        version: &str,
        fresh_for_ms: u64,
        expires_at_ms: u128,
    ) -> Result<OnlineNodeDescriptor> {
        let did = delegatee_key.delegator_did();
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did,
                public_key: delegatee_key.delegation().delegator_verification_pubkey()?,
                delegatee_public_key: delegatee_key.delegatee_public_key(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                storage_redundancy: 6,
                dht_virtual_nodes: 0,
                capabilities: OnlineNodeCapabilities::default(),
                endpoint_hint: None,
                started_at_ms: 10,
                fresh_for_ms,
                expires_at_ms,
                version: version.to_string(),
            },
            MessageSigner::new(delegatee_key, TEST_NETWORK_ID),
        )
    }

    /// Totality (#864): of two distinct descriptors of one DID at an equal lease and heartbeat,
    /// the same one is selected whatever order they are read in: two delegatee keys of one
    /// account (two processes publishing in the same millisecond).
    #[test]
    fn test_latest_valid_by_did_breaks_an_equal_lease_by_content() -> Result<()> {
        let account = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey(&account)?;
        let other_delegatee_key = DelegateeKey::new_with_seckey(&account)?;
        let one = descriptor_of(&delegatee_key, "test", 100, 200)?.heard_at(20);
        let other_process = descriptor_of(&other_delegatee_key, "test", 100, 200)?.heard_at(20);
        assert_eq!(other_process.did, one.did);
        assert_ne!(other_process, one);

        let forward = OnlineNodeDescriptor::latest_valid_by_did(
            [one.clone(), other_process.clone()],
            50,
            TEST_NETWORK_ID,
            false,
        );
        let reverse = OnlineNodeDescriptor::latest_valid_by_did(
            [other_process, one],
            50,
            TEST_NETWORK_ID,
            false,
        );
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), 1);
        Ok(())
    }

    #[test]
    fn test_descriptor_signature_covers_mutable_fields() -> Result<()> {
        let mut descriptor = descriptor_at(20, 10, 30)?;
        assert!(descriptor.verify_signature(TEST_NETWORK_ID));

        descriptor.node_type = OnlineNodeType::Browser;
        assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
        descriptor = descriptor_at(20, 10, 30)?;
        descriptor.storage_redundancy = 7;
        assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
        descriptor = descriptor_at(20, 10, 30)?;
        descriptor.expires_at_ms = 31;
        assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
        descriptor = descriptor_at(20, 10, 30)?;
        descriptor.fresh_for_ms = 11;
        assert!(!descriptor.verify_signature(TEST_NETWORK_ID));
        Ok(())
    }

    /// The heard time is neither signed nor encoded: a descriptor heard at any time verifies,
    /// and its element is the same bytes, which decode unheard.
    #[test]
    fn test_heard_time_is_neither_signed_nor_encoded() -> Result<()> {
        let unheard = descriptor_at(0, 10, 30)?;
        let heard = unheard.clone().heard_at(25);
        assert!(heard.verify_signature(TEST_NETWORK_ID));
        let element = heard.to_element()?;
        assert_eq!(element, unheard.to_element()?);
        assert_eq!(OnlineNodeDescriptor::from_element(&element)?, unheard);
        Ok(())
    }

    /// Lease law (#918): live iff `t ≤ ℓ` and `t ≤ τ* + T + σ`; a descriptor re-appended after
    /// its registrant stopped is listed at most until its signed lease end.
    #[test]
    fn test_a_descriptor_is_live_within_its_lease_and_freshness() -> Result<()> {
        let skew = TS_OFFSET_TOLERANCE_MS;
        let fresh = descriptor_at(1_000, 90, 10_000)?;
        assert!(fresh.is_live_at(1_000 + 90 + skew, TEST_NETWORK_ID));
        assert!(!fresh.is_live_at(1_000 + 90 + skew + 1, TEST_NETWORK_ID));

        let replayed = descriptor_at(10_000, 90, 10_000)?;
        assert!(replayed.is_live_at(10_000, TEST_NETWORK_ID));
        let replayed_late = descriptor_at(10_001, 90, 10_000)?;
        assert!(
            !replayed_late.is_live_at(10_001, TEST_NETWORK_ID),
            "a fresh dot never extends a lease past its signed end"
        );
        Ok(())
    }

    /// The reader bounds the signed terms (Law 9): a lease ending more than `Λ + σ` from now, or
    /// a freshness window longer than `Λ`, is never live, so a one-shot write cannot list its
    /// signer for longer than a lease; at the bounds it is live.
    #[test]
    fn test_a_descriptor_with_terms_past_the_lease_is_never_live() -> Result<()> {
        let lease = crate::descriptor::registry_lease().as_millis();
        let lease_u64 = u64::try_from(lease).unwrap_or(u64::MAX);
        let skew = TS_OFFSET_TOLERANCE_MS;
        let now = 1_000_000;

        let at_bound = descriptor_at(now, lease_u64, now + lease + skew)?;
        assert!(at_bound.is_live_at(now, TEST_NETWORK_ID));
        let long_lease = descriptor_at(now, 90_000, now + lease + skew + 1)?;
        assert!(!long_lease.is_live_at(now, TEST_NETWORK_ID));
        let long_window = descriptor_at(now, lease_u64 + 1, now + 90_000)?;
        assert!(!long_window.is_live_at(now, TEST_NETWORK_ID));
        assert!(
            long_lease.is_live_at(now + skew + 1, TEST_NETWORK_ID),
            "a lease within reach of the reader's clock is live again"
        );
        Ok(())
    }

    #[test]
    fn test_descriptor_round_trips_through_dht_encoding() -> Result<()> {
        let descriptor = descriptor_at(0, 10, 30)?;
        let element = descriptor.to_element()?;
        let decoded = OnlineNodeDescriptor::from_element(&element)?;

        assert_eq!(decoded, descriptor);
        assert!(decoded.verify_signature(TEST_NETWORK_ID));
        Ok(())
    }

    /// The newest lease of a DID wins even when an older lease was heard later, as a replayed
    /// older lease is; expired descriptors are kept only on request.
    #[test]
    fn test_latest_valid_by_did_filters_expired_and_keeps_the_newest_lease() -> Result<()> {
        let key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey(&key)?;
        let older = descriptor_of(&delegatee_key, "old", 100, 90)?.heard_at(45);
        let newer = descriptor_of(&delegatee_key, "new", 100, 100)?.heard_at(40);
        let other_live = descriptor_at(25, 100, 100)?;
        let expired = descriptor_at(30, 100, 40)?;

        let descriptors = OnlineNodeDescriptor::latest_valid_by_did(
            vec![
                older.clone(),
                newer.clone(),
                other_live.clone(),
                expired.clone(),
            ],
            50,
            TEST_NETWORK_ID,
            false,
        );

        assert_eq!(descriptors.len(), 2);
        assert!(descriptors.iter().any(|descriptor| descriptor == &newer));
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor == &other_live));

        let with_expired = OnlineNodeDescriptor::latest_valid_by_did(
            vec![older, newer, other_live, expired],
            50,
            TEST_NETWORK_ID,
            true,
        );
        assert_eq!(with_expired.len(), 3);
        Ok(())
    }
}
