#![warn(missing_docs)]
//! Authenticated `.rings` names backed by Chord storage.
//!
//! The first `.rings` namespace is deliberately self-authenticating: a name is
//! valid only when its left-most label is derived from the owner verification
//! public key. Human-readable aliases such as `alice.rings` need a separate
//! allocation and recovery protocol, so they are not accepted by this module.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use rings_core::dht::Did;
use rings_core::ecc::keccak256;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::decode_descriptor;
use crate::descriptor::encode_descriptor;
use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitTransport;

/// Pseudo-TLD served by the Rings overlay resolver.
pub const RINGS_NAME_SUFFIX: &str = ".rings";

/// Domain-separated DHT topic prefix for `.rings` records.
pub const RINGS_NAME_DHT_PREFIX: &str = "rings-name:v1";

const RINGS_NAME_SCHEMA_VERSION: u16 = 1;
const SELF_AUTH_LABEL_BYTES: usize = 20;

/// Canonical `.rings` name.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct RingsName(String);

impl RingsName {
    /// Parse and canonicalize a `.rings` name.
    pub fn parse(name: impl AsRef<str>) -> Result<Self> {
        let name = name.as_ref();
        let trimmed = name.trim().trim_end_matches('.');
        if trimmed.is_empty() {
            return Err(Error::InvalidRingsName(
                ".rings name must not be empty".to_string(),
            ));
        }

        let canonical = trimmed.to_ascii_lowercase();
        if !canonical.ends_with(RINGS_NAME_SUFFIX) {
            return Err(Error::InvalidRingsName(format!(
                "name {name:?} must end with {RINGS_NAME_SUFFIX}"
            )));
        }

        let label = canonical
            .strip_suffix(RINGS_NAME_SUFFIX)
            .expect("suffix already checked");
        validate_self_auth_label(label)?;
        Ok(Self(canonical))
    }

    /// Derive the self-authenticating `.rings` name for `owner_public_key`.
    pub fn for_owner(owner_public_key: &VerificationPublicKey) -> Self {
        Self(format!(
            "{}{RINGS_NAME_SUFFIX}",
            self_auth_label(owner_public_key)
        ))
    }

    /// Return the canonical name.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Return the DHT topic used to store records for this name on `network_id`.
    pub fn dht_topic(&self, network_id: u32) -> String {
        format!("{RINGS_NAME_DHT_PREFIX}:{network_id}:{}", self.as_str())
    }

    /// Return the Chord key used to fetch records for this name on `network_id`.
    pub fn dht_key(&self, network_id: u32) -> CoreResult<Did> {
        rings_core::dht::entry::Entry::gen_did(&self.dht_topic(network_id))
    }

    /// Return whether this name is self-authenticated by `owner_public_key`.
    pub fn matches_owner(&self, owner_public_key: &VerificationPublicKey) -> bool {
        *self == Self::for_owner(owner_public_key)
    }
}

impl TryFrom<String> for RingsName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&value).map_err(|error| error.to_string())
    }
}

impl From<RingsName> for String {
    fn from(name: RingsName) -> Self {
        name.0
    }
}

impl std::fmt::Display for RingsName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

fn validate_self_auth_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 63 {
        return Err(Error::InvalidRingsName(
            ".rings self-auth label must be 1..=63 bytes".to_string(),
        ));
    }
    if label.contains('.') {
        return Err(Error::InvalidRingsName(
            "human-readable .rings aliases are not part of v1".to_string(),
        ));
    }
    let Some(rest) = label.strip_prefix('r') else {
        return Err(Error::InvalidRingsName(
            ".rings self-auth label must start with 'r'".to_string(),
        ));
    };
    if rest.len() != SELF_AUTH_LABEL_BYTES * 2 || !rest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::InvalidRingsName(
            ".rings self-auth label must be r + 40 lowercase hex chars".to_string(),
        ));
    }
    if !rest
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(Error::InvalidRingsName(
            ".rings self-auth label must be lowercase".to_string(),
        ));
    }
    Ok(())
}

fn self_auth_label(owner_public_key: &VerificationPublicKey) -> String {
    let mut transcript = b"rings-name:v1\0".to_vec();
    transcript.extend_from_slice(&owner_public_key.transcript_bytes());
    let digest = keccak256(&transcript);
    format!("r{}", lowercase_hex(&digest[..SELF_AUTH_LABEL_BYTES]))
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Descriptor fields covered by the `.rings` name signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct RingsNameRecordBody {
    /// Canonical self-authenticating `.rings` name.
    pub name: RingsName,
    /// Account public key that owns `name`.
    pub owner_public_key: VerificationPublicKey,
    /// DID reached after resolving this name.
    pub target_did: Did,
    /// Session public key used by the target for encrypted overlay/onion setup.
    pub session_public_key: PublicKey<33>,
    /// Application service name exposed by the target.
    pub service: String,
    /// Transport class for the resolved service.
    pub transport: OnionExitTransport,
    /// Rings network identifier.
    pub network_id: u32,
    /// Monotonic version for deterministic conflict resolution.
    pub seq: u64,
    /// Record expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
}

#[derive(Serialize)]
struct RingsNameRecordBodyRef<'a> {
    schema_version: u16,
    name: &'a RingsName,
    owner_public_key: &'a VerificationPublicKey,
    target_did: Did,
    session_public_key: &'a PublicKey<33>,
    service: &'a str,
    transport: OnionExitTransport,
    network_id: u32,
    seq: u64,
    expires_at_ms: u128,
}

impl RingsNameRecordBody {
    fn body_ref(&self) -> RingsNameRecordBodyRef<'_> {
        RingsNameRecordBodyRef {
            schema_version: RINGS_NAME_SCHEMA_VERSION,
            name: &self.name,
            owner_public_key: &self.owner_public_key,
            target_did: self.target_did,
            session_public_key: &self.session_public_key,
            service: self.service.as_str(),
            transport: self.transport,
            network_id: self.network_id,
            seq: self.seq,
            expires_at_ms: self.expires_at_ms,
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        bincode::serialize(&self.body_ref()).map_err(CoreError::BincodeSerialize)
    }

    fn validate_unsigned(&self) -> Result<()> {
        if !self.name.matches_owner(&self.owner_public_key) {
            return Err(Error::InvalidRingsName(
                ".rings name does not match owner public key".to_string(),
            ));
        }
        if self.service.trim().is_empty() || self.service.trim() != self.service {
            return Err(Error::InvalidRingsName(
                ".rings service must be non-empty and trimmed".to_string(),
            ));
        }
        Ok(())
    }
}

/// Signed `.rings` name record.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct RingsNameRecord {
    /// Wire schema version covered by the signature.
    pub schema_version: u16,
    /// Canonical self-authenticating `.rings` name.
    pub name: RingsName,
    /// Account public key that owns `name`.
    pub owner_public_key: VerificationPublicKey,
    /// DID reached after resolving this name.
    pub target_did: Did,
    /// Session public key used by the target for encrypted overlay/onion setup.
    pub session_public_key: PublicKey<33>,
    /// Application service name exposed by the target.
    pub service: String,
    /// Transport class for the resolved service.
    pub transport: OnionExitTransport,
    /// Rings network identifier.
    pub network_id: u32,
    /// Monotonic version for deterministic conflict resolution.
    pub seq: u64,
    /// Record expiry timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
    /// Signature over the canonical record body.
    pub signature: MessageVerification,
}

impl RingsNameRecord {
    /// Create and sign a `.rings` name record.
    pub fn new_signed(body: RingsNameRecordBody, session_sk: &SessionSk) -> CoreResult<Self> {
        if body.owner_public_key.did() != session_sk.account_did() {
            return Err(CoreError::InvalidMessage(
                ".rings record owner/session mismatch".to_string(),
            ));
        }
        body.validate_unsigned()
            .map_err(|error| CoreError::InvalidMessage(error.to_string()))?;
        let signature = MessageVerification::new(&body.signing_data()?, session_sk)?;
        Ok(Self {
            schema_version: RINGS_NAME_SCHEMA_VERSION,
            name: body.name,
            owner_public_key: body.owner_public_key,
            target_did: body.target_did,
            session_public_key: body.session_public_key,
            service: body.service,
            transport: body.transport,
            network_id: body.network_id,
            seq: body.seq,
            expires_at_ms: body.expires_at_ms,
            signature,
        })
    }

    fn body_ref(&self) -> RingsNameRecordBodyRef<'_> {
        RingsNameRecordBodyRef {
            schema_version: self.schema_version,
            name: &self.name,
            owner_public_key: &self.owner_public_key,
            target_did: self.target_did,
            session_public_key: &self.session_public_key,
            service: self.service.as_str(),
            transport: self.transport,
            network_id: self.network_id,
            seq: self.seq,
            expires_at_ms: self.expires_at_ms,
        }
    }

    fn signing_data(&self) -> CoreResult<Vec<u8>> {
        bincode::serialize(&self.body_ref()).map_err(CoreError::BincodeSerialize)
    }

    /// Return whether this record uses the supported v1 schema.
    pub const fn has_supported_schema(&self) -> bool {
        self.schema_version == RINGS_NAME_SCHEMA_VERSION
    }

    /// Return whether this record belongs to `network_id`.
    pub const fn matches_network(&self, network_id: u32) -> bool {
        self.network_id == network_id
    }

    /// Return whether this record is expired at `now_ms`.
    pub const fn is_expired_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms <= now_ms
    }

    /// Verify schema, self-auth name binding, owner signature, and session binding.
    pub fn verify_signature(&self) -> bool {
        if !self.has_supported_schema() || !self.name.matches_owner(&self.owner_public_key) {
            return false;
        }
        if self.signature.session.account_did() != self.owner_public_key.did() {
            return false;
        }
        let Ok(session_public_key) = self.signature.session.account_verification_pubkey() else {
            return false;
        };
        if session_public_key != self.owner_public_key {
            return false;
        }
        let Ok(data) = self.signing_data() else {
            return false;
        };
        self.signature.verify(&data)
    }

    /// Return whether this record is valid and not expired at `now_ms`.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.verify_signature() && !self.is_expired_at(now_ms)
    }

    /// Select the newest valid record per `.rings` name.
    pub fn latest_valid_by_name(
        records: impl IntoIterator<Item = Self>,
        network_id: u32,
        now_ms: u128,
        include_expired: bool,
    ) -> Vec<Self> {
        let mut latest = BTreeMap::<RingsName, Self>::new();
        for record in records {
            if !record.matches_network(network_id) {
                continue;
            }
            if include_expired {
                if !record.verify_signature() {
                    continue;
                }
            } else if !record.is_live_at(now_ms) {
                continue;
            }
            match latest.entry(record.name.clone()) {
                Entry::Occupied(mut entry) => {
                    let current = entry.get();
                    if record.seq > current.seq
                        || (record.seq == current.seq
                            && record.expires_at_ms > current.expires_at_ms)
                    {
                        entry.insert(record);
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(record);
                }
            }
        }
        latest.into_values().collect()
    }
}

impl Encoder for RingsNameRecord {
    fn encode(&self) -> CoreResult<Encoded> {
        encode_descriptor(self)
    }
}

impl Decoder for RingsNameRecord {
    fn from_encoded(encoded: &Encoded) -> CoreResult<Self> {
        let record: Self = decode_descriptor(encoded)?;
        if record.has_supported_schema() {
            Ok(record)
        } else {
            Err(CoreError::Decode)
        }
    }
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;
    use rings_core::utils::get_epoch_ms;

    use super::*;

    fn session() -> SessionSk {
        SessionSk::new_with_seckey(&SecretKey::random()).unwrap()
    }

    fn body_at(session_sk: &SessionSk, now_ms: u128) -> RingsNameRecordBody {
        let owner_public_key = session_sk
            .session()
            .account_verification_pubkey()
            .expect("test session should expose account key");
        RingsNameRecordBody {
            name: RingsName::for_owner(&owner_public_key),
            owner_public_key,
            target_did: session_sk.account_did(),
            session_public_key: session_sk.session_public_key(),
            service: "web".to_string(),
            transport: OnionExitTransport::Tcp,
            network_id: 7,
            seq: 1,
            expires_at_ms: now_ms + 60_000,
        }
    }

    #[test]
    fn self_auth_name_round_trips_as_canonical_rings_name() -> Result<()> {
        let session_sk = session();
        let owner_public_key = session_sk.session().account_verification_pubkey()?;
        let name = RingsName::for_owner(&owner_public_key);
        let parsed = RingsName::parse(format!("{}.", name.as_str().to_ascii_uppercase()))?;

        assert_eq!(parsed, name);
        assert!(name.matches_owner(&owner_public_key));
        assert_eq!(
            name.dht_topic(42),
            format!("{RINGS_NAME_DHT_PREFIX}:42:{name}")
        );
        Ok(())
    }

    #[test]
    fn parser_rejects_human_aliases_in_v1() {
        assert!(matches!(
            RingsName::parse("alice.rings"),
            Err(Error::InvalidRingsName(_))
        ));
        assert!(matches!(
            RingsName::parse("alice.example.rings"),
            Err(Error::InvalidRingsName(_))
        ));
    }

    #[test]
    fn signed_record_verifies_owner_name_binding() -> CoreResult<()> {
        let session_sk = session();
        let now_ms = get_epoch_ms();
        let record = RingsNameRecord::new_signed(body_at(&session_sk, now_ms), &session_sk)?;

        assert!(record.verify_signature());
        assert!(record.is_live_at(now_ms));
        assert!(!record.is_expired_at(now_ms));
        Ok(())
    }

    #[test]
    fn record_rejects_wrong_self_auth_name() {
        let session_sk = session();
        let other = session();
        let mut body = body_at(&session_sk, get_epoch_ms());
        let other_key = other.session().account_verification_pubkey().unwrap();
        body.name = RingsName::for_owner(&other_key);

        assert!(RingsNameRecord::new_signed(body, &session_sk).is_err());
    }

    #[test]
    fn latest_valid_record_filters_network_expiry_and_stale_seq() -> CoreResult<()> {
        let session_sk = session();
        let now_ms = get_epoch_ms();
        let mut old = RingsNameRecord::new_signed(body_at(&session_sk, now_ms), &session_sk)?;
        old.seq = 1;
        let mut new_body = body_at(&session_sk, now_ms);
        new_body.seq = 2;
        let new = RingsNameRecord::new_signed(new_body, &session_sk)?;
        let mut foreign_body = body_at(&session_sk, now_ms);
        foreign_body.network_id = 8;
        foreign_body.seq = 3;
        let foreign = RingsNameRecord::new_signed(foreign_body, &session_sk)?;
        let mut expired_body = body_at(&session_sk, now_ms);
        expired_body.seq = 4;
        expired_body.expires_at_ms = now_ms;
        let expired = RingsNameRecord::new_signed(expired_body, &session_sk)?;

        let selected = RingsNameRecord::latest_valid_by_name(
            vec![old, foreign, expired, new.clone()],
            7,
            now_ms,
            false,
        );

        assert_eq!(selected, vec![new]);
        Ok(())
    }
}
