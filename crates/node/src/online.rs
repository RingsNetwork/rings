#![warn(missing_docs)]
//! Signed online-node descriptors stored in the DHT.

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::Decoder;
use rings_core::message::DhtProtocolMode;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::decode_descriptor;
use crate::descriptor::encode_descriptor;
use crate::descriptor::latest_valid_by_did;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;

/// DHT topic used for online-node registry descriptors.
pub const ONLINE_NODES_TOPIC: &str = "online_nodes";
/// Capability label for nodes that provide DHT storage.
pub const ONLINE_NODE_CAPABILITY_STORAGE: &str = "storage";
/// Capability label for nodes that provide SNARK proof services.
pub const ONLINE_NODE_CAPABILITY_SNARK: &str = "snark";

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

/// Descriptor fields covered by the online-node signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnlineNodeDescriptorBody {
    /// DID of the node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Session public key used for encrypted onion relay frames.
    pub session_public_key: PublicKey<33>,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
    /// Optional capability labels.
    pub capabilities: Vec<String>,
    /// Optional endpoint hint, controlled by node policy/configuration.
    pub endpoint_hint: Option<String>,
    /// Process start timestamp in milliseconds since Unix epoch.
    pub started_at_ms: u128,
    /// Heartbeat timestamp in milliseconds since Unix epoch.
    pub heartbeat_at_ms: u128,
    /// Expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Node software version.
    pub version: String,
}

impl OnlineNodeDescriptorBody {
    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        OnlineNodeDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            session_public_key: &self.session_public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
            capabilities: &self.capabilities,
            endpoint_hint: &self.endpoint_hint,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
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

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_signing_data(&self) -> Result<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        OnlineNodeDescriptor {
            did: self.did,
            public_key: self.public_key,
            session_public_key: self.session_public_key,
            node_type: self.node_type,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
            capabilities: self.capabilities,
            endpoint_hint: self.endpoint_hint,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
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
    session_public_key: &'a PublicKey<33>,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    capabilities: &'a [String],
    endpoint_hint: &'a Option<String>,
    started_at_ms: u128,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    version: &'a str,
}

impl OnlineNodeDescriptorBodyRef<'_> {
    fn signing_data(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(Error::BincodeSerialize)
    }
}

/// Signed descriptor published by online nodes.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnlineNodeDescriptor {
    /// DID of the node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Session public key used for encrypted onion relay frames.
    pub session_public_key: PublicKey<33>,
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Storage redundancy required by this DHT protocol mode.
    pub storage_redundancy: u16,
    /// Storage virtual-node positions required by this DHT protocol mode.
    pub dht_virtual_nodes: u16,
    /// Optional capability labels.
    pub capabilities: Vec<String>,
    /// Optional endpoint hint, controlled by node policy/configuration.
    pub endpoint_hint: Option<String>,
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

impl OnlineNodeDescriptor {
    /// Create and sign a descriptor.
    pub fn new_signed(body: OnlineNodeDescriptorBody, session_sk: &SessionSk) -> Result<Self> {
        sign_descriptor_body(
            body,
            session_sk,
            "online node descriptor DID/public key/session mismatch",
        )
    }

    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            session_public_key,
            node_type,
            network_id,
            storage_redundancy,
            dht_virtual_nodes,
            capabilities,
            endpoint_hint,
            started_at_ms,
            heartbeat_at_ms,
            expires_at_ms,
            version,
            signature: _,
        } = self;

        OnlineNodeDescriptorBodyRef {
            did: *did,
            public_key,
            session_public_key,
            node_type,
            network_id: *network_id,
            storage_redundancy: *storage_redundancy,
            dht_virtual_nodes: *dht_virtual_nodes,
            capabilities,
            endpoint_hint,
            started_at_ms: *started_at_ms,
            heartbeat_at_ms: *heartbeat_at_ms,
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
    pub const fn matches_dht_protocol(&self, expected: DhtProtocolMode) -> bool {
        self.dht_protocol_mode().matches(expected)
    }

    /// Verify the descriptor signature and DID/public-key binding.
    ///
    /// This does not apply the embedded [`MessageVerification`] timestamp/TTL
    /// as a liveness rule. Online-node liveness is defined solely by the
    /// signed `expires_at_ms` descriptor field; use [`Self::is_live_at`] when
    /// expiry should be enforced.
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

    /// Select the newest valid descriptor per DID.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_did(descriptors, now_ms, include_expired)
    }
}

impl SignedDescriptor for OnlineNodeDescriptor {
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

    fn descriptor_signing_data(&self) -> Result<Vec<u8>> {
        self.signing_data()
    }
}

impl Encoder for OnlineNodeDescriptor {
    fn encode(&self) -> Result<Encoded> {
        encode_descriptor(self)
    }
}

impl Decoder for OnlineNodeDescriptor {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        decode_descriptor(encoded)
    }
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use super::*;

    fn descriptor_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> Result<OnlineNodeDescriptor> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did,
                public_key: session_sk.session().account_verification_pubkey()?,
                session_public_key: session_sk.session_public_key(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                storage_redundancy: 6,
                dht_virtual_nodes: 0,
                capabilities: vec![ONLINE_NODE_CAPABILITY_STORAGE.to_string()],
                endpoint_hint: None,
                started_at_ms: 10,
                heartbeat_at_ms,
                expires_at_ms,
                version: "test".to_string(),
            },
            &session_sk,
        )
    }

    #[test]
    fn descriptor_signature_covers_mutable_fields() -> Result<()> {
        let mut descriptor = descriptor_at(20, 30)?;
        assert!(descriptor.verify_signature());

        descriptor.node_type = OnlineNodeType::Browser;
        assert!(!descriptor.verify_signature());
        descriptor = descriptor_at(20, 30)?;
        descriptor.storage_redundancy = 7;
        assert!(!descriptor.verify_signature());
        Ok(())
    }

    #[test]
    fn descriptor_round_trips_through_dht_encoding() -> Result<()> {
        let descriptor = descriptor_at(20, 30)?;
        let encoded = descriptor.encode()?;
        let decoded = OnlineNodeDescriptor::from_encoded(&encoded)?;

        assert_eq!(decoded, descriptor);
        assert!(decoded.verify_signature());
        Ok(())
    }

    #[test]
    fn latest_valid_by_did_filters_expired_and_keeps_newest() -> Result<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        let public_key = session_sk.session().account_verification_pubkey()?;

        let older = OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did,
                public_key: public_key.clone(),
                session_public_key: session_sk.session_public_key(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                storage_redundancy: 6,
                dht_virtual_nodes: 0,
                capabilities: vec![],
                endpoint_hint: None,
                started_at_ms: 1,
                heartbeat_at_ms: 10,
                expires_at_ms: 100,
                version: "old".to_string(),
            },
            &session_sk,
        )?;
        let newer = OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did,
                public_key,
                session_public_key: session_sk.session_public_key(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                storage_redundancy: 6,
                dht_virtual_nodes: 0,
                capabilities: vec![],
                endpoint_hint: None,
                started_at_ms: 1,
                heartbeat_at_ms: 20,
                expires_at_ms: 100,
                version: "new".to_string(),
            },
            &session_sk,
        )?;
        let other_live = descriptor_at(25, 100)?;
        let expired = descriptor_at(30, 40)?;

        let descriptors = OnlineNodeDescriptor::latest_valid_by_did(
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

        let with_expired = OnlineNodeDescriptor::latest_valid_by_did(
            vec![older, newer, other_live, expired],
            50,
            true,
        );
        assert_eq!(with_expired.len(), 3);
        Ok(())
    }
}
