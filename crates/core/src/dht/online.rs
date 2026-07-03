#![warn(missing_docs)]
//! Signed online-node descriptors stored in the DHT.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;
use crate::ecc::VerificationPublicKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Decoder;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessageVerification;
use crate::session::SessionSk;

/// DHT topic used for online-node registry descriptors.
pub const ONLINE_NODES_TOPIC: &str = "online_nodes";

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

#[derive(Serialize)]
struct OnlineNodeDescriptorSigningData<'a> {
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

impl OnlineNodeDescriptor {
    /// Create and sign a descriptor.
    #[allow(clippy::too_many_arguments)]
    pub fn new_signed(
        did: Did,
        public_key: VerificationPublicKey,
        node_type: OnlineNodeType,
        network_id: u32,
        capabilities: Vec<String>,
        endpoint_hint: Option<String>,
        started_at_ms: u128,
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
        version: String,
        session_sk: &SessionSk,
    ) -> Result<Self> {
        if public_key.did() != did || session_sk.account_did() != did {
            return Err(Error::InvalidMessage(
                "online node descriptor DID/public key/session mismatch".to_string(),
            ));
        }

        let placeholder = MessageVerification::new(&[], session_sk)?;
        let mut descriptor = Self {
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
            signature: placeholder,
        };
        descriptor.signature = MessageVerification::new(&descriptor.signing_data()?, session_sk)?;
        Ok(descriptor)
    }

    fn signing_data(&self) -> Result<Vec<u8>> {
        let data = OnlineNodeDescriptorSigningData {
            did: self.did,
            public_key: &self.public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            capabilities: &self.capabilities,
            endpoint_hint: &self.endpoint_hint,
            started_at_ms: self.started_at_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
            expires_at_ms: self.expires_at_ms,
            version: &self.version,
        };
        bincode::serialize(&data).map_err(Error::BincodeSerialize)
    }

    /// Verify the descriptor signature and DID/public-key binding.
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
            if !descriptor.verify_signature() {
                continue;
            }
            if !include_expired && descriptor.is_expired_at(now_ms) {
                continue;
            }
            latest
                .entry(descriptor.did)
                .and_modify(|current| {
                    if descriptor.heartbeat_at_ms > current.heartbeat_at_ms {
                        *current = descriptor.clone();
                    }
                })
                .or_insert(descriptor);
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
    use super::*;
    use crate::ecc::SecretKey;
    use crate::session::SessionSk;

    fn descriptor_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> Result<OnlineNodeDescriptor> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        OnlineNodeDescriptor::new_signed(
            did,
            session_sk.session().account_verification_pubkey()?,
            OnlineNodeType::Native,
            1,
            vec!["storage".to_string()],
            None,
            10,
            heartbeat_at_ms,
            expires_at_ms,
            "test".to_string(),
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
            did,
            public_key.clone(),
            OnlineNodeType::Native,
            1,
            vec![],
            None,
            1,
            10,
            100,
            "old".to_string(),
            &session_sk,
        )?;
        let newer = OnlineNodeDescriptor::new_signed(
            did,
            public_key,
            OnlineNodeType::Native,
            1,
            vec![],
            None,
            1,
            20,
            100,
            "new".to_string(),
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
