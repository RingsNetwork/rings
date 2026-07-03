#![warn(missing_docs)]
//! Signed online-node descriptors stored in the DHT.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error;
use rings_core::error::Result;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

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
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
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
    fn validate_signer(&self, session_sk: &SessionSk) -> Result<()> {
        if self.public_key.did() != self.did || session_sk.account_did() != self.did {
            return Err(Error::InvalidMessage(
                "online node descriptor DID/public key/session mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        OnlineNodeDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
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

#[derive(Serialize)]
struct OnlineNodeDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    node_type: &'a OnlineNodeType,
    network_id: u32,
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
    /// Runtime family of this node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
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
        body.validate_signer(session_sk)?;
        let signature = MessageVerification::new(&body.signing_data()?, session_sk)?;
        Ok(Self {
            did: body.did,
            public_key: body.public_key,
            node_type: body.node_type,
            network_id: body.network_id,
            capabilities: body.capabilities,
            endpoint_hint: body.endpoint_hint,
            started_at_ms: body.started_at_ms,
            heartbeat_at_ms: body.heartbeat_at_ms,
            expires_at_ms: body.expires_at_ms,
            version: body.version,
            signature,
        })
    }

    fn body_ref(&self) -> OnlineNodeDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            node_type,
            network_id,
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
            node_type,
            network_id: *network_id,
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

    /// Return whether this descriptor belongs to `network_id`.
    pub const fn matches_network(&self, network_id: u32) -> bool {
        self.network_id == network_id
    }

    /// Verify the descriptor signature and DID/public-key binding.
    ///
    /// This does not apply the embedded [`MessageVerification`] timestamp/TTL
    /// as a liveness rule. Online-node liveness is defined solely by the
    /// signed `expires_at_ms` descriptor field; use [`Self::is_live_at`] when
    /// expiry should be enforced.
    pub fn verify_signature(&self) -> bool {
        if self.public_key.did() != self.did || self.signature.session.account_did() != self.did {
            return false;
        }

        let Ok(session_public_key) = self.signature.session.account_verification_pubkey() else {
            return false;
        };
        if session_public_key != self.public_key {
            return false;
        }

        let Ok(data) = self.signing_data() else {
            return false;
        };
        self.signature.verify(&data)
    }

    /// Returns whether this descriptor is expired at `now_ms`.
    pub fn is_expired_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms < now_ms
    }

    /// Returns whether this descriptor has a valid signature and is not expired.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.verify_signature() && !self.is_expired_at(now_ms)
    }

    /// Select the newest valid descriptor per DID.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        include_expired: bool,
    ) -> Vec<Self> {
        let mut latest = BTreeMap::<Did, Self>::new();
        for descriptor in descriptors {
            if include_expired {
                if !descriptor.verify_signature() {
                    continue;
                }
            } else if !descriptor.is_live_at(now_ms) {
                continue;
            }
            match latest.entry(descriptor.did) {
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

impl Encoder for OnlineNodeDescriptor {
    fn encode(&self) -> Result<Encoded> {
        bincode::serialize(self)
            .map_err(Error::BincodeSerialize)?
            .encode()
    }
}

impl Decoder for OnlineNodeDescriptor {
    fn from_encoded(encoded: &Encoded) -> Result<Self> {
        let data: Vec<u8> = encoded.decode()?;
        bincode::deserialize(&data).map_err(Error::BincodeDeserialize)
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
                node_type: OnlineNodeType::Native,
                network_id: 1,
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
                node_type: OnlineNodeType::Native,
                network_id: 1,
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
                node_type: OnlineNodeType::Native,
                network_id: 1,
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
