//! DHT-registered TURN relay service discovery (`TurnServiceDescriptor`, `TurnServiceRegistration`)
//! and internal control-plane reservation protocol (`TurnReservationRequest`) for multi-hop
//! AEZ-backed TURN onion relays (#1292).
//!
//! Native nodes that host or bridge a `rings-turn` server publish a signed [`TurnServiceDescriptor`]
//! onto [`TURN_SERVICES_TOPIC`], advertising their connectable `turn://` endpoint, `server_pubkey`,
//! and whether they support multi-hop `ChainedHop` onion relay reservations.
//!
//! Both browser WASM nodes and native nodes can:
//! 1. Discover live [`TurnServiceDescriptor`]s from the DHT via
//!    [`Processor::lookup_turn_services`](crate::processor::Processor::lookup_turn_services).
//! 2. Format discovered descriptors into `?pubkey=<base58(server_pubkey)>` ICE server URLs for
//!    direct signature-authenticated TURN allocation, or
//! 3. Reserve an `N`-hop unlinkable `SessionTicket` + `rings-aez` (`τ = 0`) TURN onion circuit
//!    via [`TurnReservationRequest`], receiving a standard `turn://<ticket_user>:<ticket_pass>@<T_1>`
//!    ICE server URL compatible with unmodified browser `RTCPeerConnection` and `webrtc-rs`.

use std::borrow::Borrow;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::PublicKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_turn::extract_turn_server_pubkey;
use rings_turn::format_turn_server_url;
use rings_turn::TurnCipherDirection;
use rings_turn::TurnCredentials;
use serde::Deserialize;
use serde::Serialize;

use crate::descriptor::heard_descriptors;
use crate::descriptor::latest_valid_by_did;
use crate::descriptor::sign_descriptor_body;
use crate::descriptor::RegistryElement;
use crate::descriptor::SignedDescriptor;
use crate::descriptor::SignedDescriptorBody;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::entry;
use crate::registration::validate_schedule;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::LeaseTerms;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::registration::TimingKeys;

/// Global DHT registry topic on which TURN-capable nodes publish [`TurnServiceDescriptor`]s.
pub const TURN_SERVICES_TOPIC: &str = "turn_services";

/// Message family of the TURN service descriptor signature.
const TURN_SERVICE_DESCRIPTOR_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-node:turn-service-descriptor");

/// Default heartbeat interval for TURN service descriptor republication.
pub const DEFAULT_TURN_SERVICE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Default freshness window (TTL) for TURN service descriptors (three heartbeats).
pub const DEFAULT_TURN_SERVICE_TTL: Duration = Duration::from_secs(90);

/// Validate and normalize a connectable `turn://` or `turns://` base URL together with its
/// secp256k1 `server_pubkey`.
pub fn validate_turn_service_endpoint(
    turn_url: &str,
    server_pubkey: PublicKey<33>,
) -> Result<String> {
    let trimmed = turn_url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(Error::InvalidConfig(format!(
            "turn_url must start with turn:// or turns://, got {trimmed:?}"
        )));
    };
    if !scheme.eq_ignore_ascii_case("turn") && !scheme.eq_ignore_ascii_case("turns") {
        return Err(Error::InvalidConfig(format!(
            "turn_url scheme must be turn or turns, got {scheme:?}"
        )));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..authority_end).unwrap_or(rest);
    if authority.is_empty() || authority.contains('@') {
        return Err(Error::InvalidConfig(format!(
            "turn_url must carry a bare host:port authority without userinfo, got {trimmed:?}"
        )));
    }
    let formatted = format_turn_server_url(trimmed, server_pubkey)
        .map_err(|e| Error::InvalidConfig(format!("invalid TURN server_pubkey: {e}")))?;
    if extract_turn_server_pubkey(&formatted) != Some(server_pubkey) {
        return Err(Error::InvalidConfig(
            "failed to round-trip TURN server_pubkey in turn_url".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Descriptor fields covered by the [`TurnServiceDescriptor`] signature (#1292).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct TurnServiceDescriptorBody {
    /// DID of the node hosting or bridging this TURN service.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Overlay network identifier.
    pub network_id: u32,
    /// Connectable TURN listener URL (`turn://host:port` or `turns://host:port`).
    pub turn_url: String,
    /// The TURN server's compressed secp256k1 public key (`server_pubkey`) used for ECDH
    /// credential derivation and server-grant verification.
    pub server_pubkey: PublicKey<33>,
    /// Whether this TURN service accepts multi-hop `ChainedHop` onion relay reservations.
    pub supports_chained_relay: bool,
    /// Freshness window `T` in milliseconds.
    pub fresh_for_ms: u64,
    /// Lease end `ℓ` in milliseconds since Unix epoch.
    pub expires_at_ms: u128,
}

impl TurnServiceDescriptorBody {
    fn signing_data(&self) -> rings_core::error::Result<Vec<u8>> {
        rings_codec::serialize(self).map_err(rings_core::error::Error::CodecSerialize)
    }
}

impl SignedDescriptorBody for TurnServiceDescriptorBody {
    type Descriptor = TurnServiceDescriptor;

    const DOMAIN_TAG: DomainTag = TURN_SERVICE_DESCRIPTOR_DOMAIN_TAG;

    fn body_did(&self) -> Did {
        self.did
    }

    fn body_public_key(&self) -> &VerificationPublicKey {
        &self.public_key
    }

    fn body_network_id(&self) -> u32 {
        self.network_id
    }

    fn body_signing_data(&self) -> rings_core::error::Result<Vec<u8>> {
        self.signing_data()
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        TurnServiceDescriptor {
            body: self,
            signature,
            heartbeat_at_ms: 0,
        }
    }
}

/// Signed DHT descriptor advertising a Rings TURN service endpoint and its `server_pubkey` (#1292).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct TurnServiceDescriptor {
    /// The signed descriptor fields.
    pub body: TurnServiceDescriptorBody,
    /// Signature covering `body`, signed by the hosting node's session [`DelegateeKey`].
    pub signature: MessageVerification,
    /// Local observation timestamp (`dot` of the registry element), neither signed nor wire-encoded.
    #[serde(skip)]
    pub heartbeat_at_ms: u128,
}

impl TurnServiceDescriptor {
    /// Create and sign a [`TurnServiceDescriptor`].
    pub fn new_signed(
        body: TurnServiceDescriptorBody,
        signer: MessageSigner<impl Borrow<DelegateeKey>>,
    ) -> rings_core::error::Result<Self> {
        sign_descriptor_body(
            body,
            signer,
            "turn service descriptor DID/public key/session mismatch",
        )
    }

    /// Return whether this descriptor's `turn_url` and `server_pubkey` are well-formed.
    pub fn has_valid_endpoint(&self) -> bool {
        validate_turn_service_endpoint(&self.body.turn_url, self.body.server_pubkey).is_ok()
    }

    /// Format this descriptor as an unsigned `turn://host:port?pubkey=<base58(server_pubkey)>` URL
    /// suitable for [`rings_turn::TurnCredentialSigner::sign_ice_servers`].
    pub fn ice_server_base_url(&self) -> Result<String> {
        validate_turn_service_endpoint(&self.body.turn_url, self.body.server_pubkey)?;
        format_turn_server_url(&self.body.turn_url, self.body.server_pubkey)
            .map_err(Error::CoreError)
    }

    /// Filter and deduplicate `descriptors` to the latest valid descriptor per node [`Did`] under
    /// `network_id` at `now_ms`.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_did(
            descriptors.into_iter().filter(Self::has_valid_endpoint),
            now_ms,
            network_id,
            include_expired,
        )
    }
}

impl SignedDescriptor for TurnServiceDescriptor {
    type Body = TurnServiceDescriptorBody;

    fn descriptor_did(&self) -> Did {
        self.body.did
    }

    fn descriptor_public_key(&self) -> &VerificationPublicKey {
        &self.body.public_key
    }

    fn descriptor_network_id(&self) -> u32 {
        self.body.network_id
    }

    fn descriptor_signature(&self) -> &MessageVerification {
        &self.signature
    }

    fn descriptor_heartbeat_at_ms(&self) -> u128 {
        self.heartbeat_at_ms
    }

    fn descriptor_expires_at_ms(&self) -> u128 {
        self.body.expires_at_ms
    }

    fn descriptor_fresh_for_ms(&self) -> u64 {
        self.body.fresh_for_ms
    }

    fn descriptor_signing_data(&self) -> rings_core::error::Result<Vec<u8>> {
        self.body.signing_data()
    }

    fn heard_at(self, heartbeat_at_ms: u128) -> Self {
        Self {
            heartbeat_at_ms,
            ..self
        }
    }
}

impl RegistryElement for TurnServiceDescriptor {}

/// Configuration offer for publishing a [`TurnServiceDescriptor`] to the DHT on every heartbeat
/// (#1292).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnServiceOffer {
    turn_url: String,
    server_pubkey: PublicKey<33>,
    supports_chained_relay: bool,
    heartbeat_interval: Duration,
    ttl: Duration,
}

impl TurnServiceOffer {
    /// Construct a validated [`TurnServiceOffer`] with default heartbeat (`30s`) and TTL (`90s`).
    pub fn new(
        turn_url: impl Into<String>,
        server_pubkey: PublicKey<33>,
        supports_chained_relay: bool,
    ) -> Result<Self> {
        Self::with_schedule(
            turn_url,
            server_pubkey,
            supports_chained_relay,
            DEFAULT_TURN_SERVICE_HEARTBEAT_INTERVAL,
            DEFAULT_TURN_SERVICE_TTL,
        )
    }

    /// Construct a validated [`TurnServiceOffer`] with explicit `heartbeat_interval` and `ttl`.
    pub fn with_schedule(
        turn_url: impl Into<String>,
        server_pubkey: PublicKey<33>,
        supports_chained_relay: bool,
        heartbeat_interval: Duration,
        ttl: Duration,
    ) -> Result<Self> {
        let raw_url = turn_url.into();
        let normalized_url = validate_turn_service_endpoint(&raw_url, server_pubkey)?;
        let keys = TimingKeys {
            heartbeat_interval: "turn_service_heartbeat_interval",
            ttl: "turn_service_ttl",
            enabled_by: None,
        };
        validate_schedule(&keys, heartbeat_interval, ttl)?;
        Ok(Self {
            turn_url: normalized_url,
            server_pubkey,
            supports_chained_relay,
            heartbeat_interval,
            ttl,
        })
    }

    /// Return the connectable TURN base URL (`turn://host:port` or `turns://host:port`).
    pub fn turn_url(&self) -> &str {
        &self.turn_url
    }

    /// Return the TURN server's secp256k1 [`PublicKey<33>`].
    pub const fn server_pubkey(&self) -> PublicKey<33> {
        self.server_pubkey
    }

    /// Return whether this service accepts multi-hop `ChainedHop` onion relay reservations.
    pub const fn supports_chained_relay(&self) -> bool {
        self.supports_chained_relay
    }

    /// Return the descriptor republication heartbeat interval.
    pub const fn heartbeat_interval(&self) -> Duration {
        self.heartbeat_interval
    }

    /// Return the descriptor freshness window (TTL).
    pub const fn ttl(&self) -> Duration {
        self.ttl
    }
}

/// Periodic DHT registration task publishing a node's [`TurnServiceDescriptor`] onto
/// [`TURN_SERVICES_TOPIC`] (#1292).
#[derive(Clone, Debug)]
pub(crate) struct TurnServiceRegistration {
    offer: TurnServiceOffer,
    publisher: DhtRegistrationPublisher<[TurnServiceDescriptor; 1]>,
}

impl TurnServiceRegistration {
    /// Construct a new [`TurnServiceRegistration`] for `offer`.
    pub(crate) fn new(offer: TurnServiceOffer) -> Self {
        Self {
            publisher: DhtRegistrationPublisher::new(
                TURN_SERVICES_TOPIC.to_string(),
                offer.heartbeat_interval,
                offer.ttl,
            ),
            offer,
        }
    }

    /// Decode all well-formed [`TurnServiceDescriptor`]s from a DHT [`entry::Entry`].
    pub(crate) fn descriptors_from_entry(entry: &entry::Entry) -> Vec<TurnServiceDescriptor> {
        heard_descriptors(entry)
    }

    pub(crate) fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<TurnServiceDescriptor> {
        let body = TurnServiceDescriptorBody {
            did: context.did(),
            public_key: context.delegator_verification_pubkey()?,
            network_id: context.network_id(),
            turn_url: self.offer.turn_url.clone(),
            server_pubkey: self.offer.server_pubkey,
            supports_chained_relay: self.offer.supports_chained_relay,
            fresh_for_ms: terms.fresh_for_ms,
            expires_at_ms: terms.expires_at_ms,
        };
        TurnServiceDescriptor::new_signed(body, context.message_signer()).map_err(Error::CoreError)
    }
}

#[cfg_attr(rings_browser, async_trait(?Send))]
#[cfg_attr(rings_native, async_trait)]
impl RegistrationTask for TurnServiceRegistration {
    fn name(&self) -> &'static str {
        "turn-service"
    }

    fn interval(&self) -> Duration {
        self.offer.heartbeat_interval
    }

    async fn register_once(&self, context: &RegistrationContext<'_>) -> Result<()> {
        self.publisher
            .publish_lease(context, context.now_ms(), |terms| {
                self.descriptor_at(context, terms)
                    .map(|descriptor| [descriptor])
            })
            .await
            .map(drop)
    }
}

/// Serializable specification of one 48-byte `rings-aez` (`τ = 0`) permutation layer (#1292).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAezLayerSpec {
    /// 48-byte AEZ key (`rings_aez::KEY_BYTES`).
    pub key: Vec<u8>,
    /// Outbound permutation direction (`Encipher` or `Decipher`).
    pub outbound_direction: TurnCipherDirection,
}

impl TurnAezLayerSpec {
    /// Convert a compiled [`rings_turn::TurnPayloadCipher`] into serializable layer specifications.
    pub fn from_cipher(cipher: &rings_turn::TurnPayloadCipher) -> Vec<Self> {
        cipher
            .to_layer_specs()
            .into_iter()
            .map(|(k, outbound_direction)| Self {
                key: k.to_vec(),
                outbound_direction,
            })
            .collect()
    }

    /// Compile a slice of [`TurnAezLayerSpec`]s into an optional [`rings_turn::TurnPayloadCipher`].
    pub fn to_cipher(specs: &[Self]) -> Result<Option<rings_turn::TurnPayloadCipher>> {
        if specs.is_empty() {
            return Ok(None);
        }
        let mut raw_layers = Vec::with_capacity(specs.len());
        for spec in specs {
            let key: [u8; rings_aez::KEY_BYTES] = spec.key.as_slice().try_into().map_err(|_| {
                Error::InvalidConfig(format!(
                    "expected {}-byte AEZ key in TurnAezLayerSpec, got {} bytes",
                    rings_aez::KEY_BYTES,
                    spec.key.len()
                ))
            })?;
            raw_layers.push((key, spec.outbound_direction));
        }
        rings_turn::TurnPayloadCipher::from_layers(raw_layers)
            .map(Some)
            .map_err(Error::CoreError)
    }
}

/// Control-plane request sent over the Rings overlay or Onion invoke pipeline to reserve an
/// unlinkable TURN session on a DHT-registered TURN service node (#1292).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnReservationRequest {
    /// Reserve a final exit TURN session (`RelayBackend::LocalExit`).
    ReserveExit {
        /// Single-use secp256k1 public key for the caller or preceding hop.
        ephemeral_client_pubkey: PublicKey<33>,
        /// Requested reservation TTL in milliseconds.
        ttl_ms: u64,
        /// Optional AEZ (`τ = 0`) layer stack to install on the exit allocation.
        cipher_layers: Vec<TurnAezLayerSpec>,
    },
    /// Reserve an intermediate or entry chained TURN hop (`RelayBackend::ChainedHop`).
    ReserveChainedHop {
        /// Single-use secp256k1 public key for the caller or preceding hop.
        ephemeral_client_pubkey: PublicKey<33>,
        /// Connectable TURN listener address of the next hop `T_{i+1}`.
        next_hop_addr: std::net::SocketAddr,
        /// Unlinkable [`TurnCredentials`] for authenticating `T_i` to `T_{i+1}`.
        next_hop_credentials: TurnCredentials,
        /// Expected `XOR-RELAYED-ADDRESS` (`E_exit`) projected by `T_{i+1}`.
        projected_relay_addr: std::net::SocketAddr,
        /// Requested reservation TTL in milliseconds.
        ttl_ms: u64,
        /// Optional AEZ (`τ = 0`) layer stack to install on this hop.
        cipher_layers: Vec<TurnAezLayerSpec>,
    },
}

/// Native TURN reservation handler and multi-hop circuit builder (#1292).
#[cfg(rings_native)]
pub struct TurnReservationService;

#[cfg(rings_native)]
impl TurnReservationService {
    /// Execute a [`TurnReservationRequest`] against a local [`rings_turn::TurnServer`].
    pub async fn handle_request(
        server: &rings_turn::TurnServer,
        request: TurnReservationRequest,
    ) -> Result<rings_turn::TurnSessionReservation> {
        match request {
            TurnReservationRequest::ReserveExit {
                ephemeral_client_pubkey,
                ttl_ms,
                cipher_layers,
            } => {
                let cipher = TurnAezLayerSpec::to_cipher(&cipher_layers)?;
                server
                    .reserve_exit_session(
                        ephemeral_client_pubkey,
                        Duration::from_millis(ttl_ms.max(1)),
                        cipher,
                    )
                    .await
                    .map_err(|e| Error::InvalidConfig(format!("TURN exit reservation failed: {e}")))
            }
            TurnReservationRequest::ReserveChainedHop {
                ephemeral_client_pubkey,
                next_hop_addr,
                next_hop_credentials,
                projected_relay_addr,
                ttl_ms,
                cipher_layers,
            } => {
                let cipher = TurnAezLayerSpec::to_cipher(&cipher_layers)?;
                server
                    .reserve_chained_hop(rings_turn::TurnChainedHopSpec {
                        ephemeral_client_pubkey,
                        next_hop_addr,
                        next_hop_credentials,
                        projected_relay_addr,
                        cipher,
                        ttl: Duration::from_millis(ttl_ms.max(1)),
                    })
                    .await
                    .map_err(|e| {
                        Error::InvalidConfig(format!("TURN chained hop reservation failed: {e}"))
                    })
            }
        }
    }

    /// Reserve an `N`-hop (`N >= 1`) AEZ-permuted TURN onion circuit backwards from exit `hops[N-1]`
    /// down to entry `hops[0]`, returning `(entry_ice_server_url, exit_relay_addr, entry_credentials)`.
    ///
    /// The returned `entry_ice_server_url` (`turn://<ticket_user>:<ticket_pass>@<T_1_addr>`) can be
    /// passed directly into a browser WASM `WebSysWebrtcTransport` or native `WebrtcTransport`!
    pub async fn reserve_onion_circuit(
        hops: &[&rings_turn::TurnServer],
        link_keys: &[[u8; rings_aez::KEY_BYTES]],
        ttl: Duration,
    ) -> Result<(String, std::net::SocketAddr, TurnCredentials)> {
        if hops.is_empty() {
            return Err(Error::InvalidConfig(
                "TURN onion circuit requires at least 1 hop".to_string(),
            ));
        }
        if link_keys.len() + 1 != hops.len() {
            return Err(Error::InvalidConfig(format!(
                "expected {} AEZ link keys for {}-hop TURN circuit, got {}",
                hops.len() - 1,
                hops.len(),
                link_keys.len()
            )));
        }
        let hop_ciphers = if link_keys.is_empty() {
            vec![rings_turn::TurnPayloadCipher::identity()]
        } else {
            rings_turn::derive_entry_wrapped_hop_ciphers(link_keys).map_err(Error::CoreError)?
        };
        let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);

        let mut reversed_hops = hops.iter().copied().zip(hop_ciphers.iter()).rev();
        let Some((exit_server, exit_cipher)) = reversed_hops.next() else {
            return Err(Error::InvalidConfig(
                "TURN onion circuit requires at least 1 hop".to_string(),
            ));
        };
        let exit_eph_key = rings_core::ecc::SecretKey::random();
        let exit_res = Self::handle_request(exit_server, TurnReservationRequest::ReserveExit {
            ephemeral_client_pubkey: exit_eph_key.pubkey(),
            ttl_ms,
            cipher_layers: TurnAezLayerSpec::from_cipher(exit_cipher),
        })
        .await?;
        let exit_relay_addr = exit_res.relay_addr;
        let mut next_hop_addr = exit_res.server_addr;
        let mut next_hop_creds = exit_res
            .ticket
            .to_turn_credentials(&exit_eph_key, exit_res.server_pubkey)
            .map_err(Error::CoreError)?;

        for (hop_server, hop_cipher) in reversed_hops {
            let hop_eph_key = rings_core::ecc::SecretKey::random();
            let hop_res =
                Self::handle_request(hop_server, TurnReservationRequest::ReserveChainedHop {
                    ephemeral_client_pubkey: hop_eph_key.pubkey(),
                    next_hop_addr,
                    next_hop_credentials: next_hop_creds,
                    projected_relay_addr: exit_relay_addr,
                    ttl_ms,
                    cipher_layers: TurnAezLayerSpec::from_cipher(hop_cipher),
                })
                .await?;
            next_hop_addr = hop_res.server_addr;
            next_hop_creds = hop_res
                .ticket
                .to_turn_credentials(&hop_eph_key, hop_res.server_pubkey)
                .map_err(Error::CoreError)?;
        }

        let entry_base_url = format!("turn://{next_hop_addr}");
        let entry_ice_server_url = next_hop_creds.apply_to_ice_server_url(&entry_base_url);
        Ok((entry_ice_server_url, exit_relay_addr, next_hop_creds))
    }
}

#[cfg(all(test, rings_native))]
mod tests {
    use std::sync::Arc;

    use rings_core::ecc::SecretKey;
    use rings_core::utils::ManualClock;
    use rings_turn::TurnAdmissionPolicy;
    use rings_turn::TurnCredentialSigner;
    use rings_turn::TurnServer;
    use rings_turn::TurnServerConfig;

    use super::*;
    use crate::processor::ProcessorBuilder;
    use crate::processor::ProcessorConfig;

    #[tokio::test]
    async fn turn_service_registration_publishes_and_discovers_signed_descriptors_in_dht(
    ) -> Result<()> {
        let t0: u128 = 1_800_000_000_000;
        let clock = Arc::new(ManualClock::new(t0));
        let account_key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, 600_000, t0).unwrap();
        let turn_server_pubkey = SecretKey::random().pubkey();

        let offer = TurnServiceOffer::new("turn://turn.rings.rs:3478", turn_server_pubkey, true)?;
        let config =
            ProcessorConfig::new(1, "".to_string(), delegatee_key.clone(), 15).with_clock(clock);
        let processor = ProcessorBuilder::from_config(&config)?
            .announce_turn_service(offer.clone())
            .build_at(t0)?;

        // Trigger the registration tasks once so `TurnServiceRegistration` publishes to the DHT.
        processor.register_all_once_for_test().await?;

        let discovered = processor.lookup_turn_services(false).await?;
        assert_eq!(discovered.len(), 1);
        let desc = &discovered[0];
        assert_eq!(desc.body.did, processor.did());
        assert_eq!(desc.body.turn_url, "turn://turn.rings.rs:3478");
        assert_eq!(desc.body.server_pubkey, turn_server_pubkey);
        assert!(desc.body.supports_chained_relay);

        // Verify that the discovered descriptor formats into a signable `?pubkey=...` URL.
        let base_url = desc.ice_server_base_url()?;
        let signed_url = delegatee_key
            .sign_ice_servers(&base_url)
            .map_err(Error::CoreError)?;
        assert!(signed_url.starts_with("turn://"));
        assert!(signed_url.ends_with("@turn.rings.rs:3478"));
        assert!(!signed_url.contains("pubkey="));
        Ok(())
    }

    #[tokio::test]
    async fn turn_reservation_service_builds_working_three_hop_onion_ice_server_url() -> Result<()>
    {
        let make_strict_config = || {
            let mut cfg = TurnServerConfig::loopback_ephemeral();
            cfg.admission = TurnAdmissionPolicy::allowlist(std::iter::empty());
            cfg
        };
        let hop1 = TurnServer::start(make_strict_config())
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let hop2 = TurnServer::start(make_strict_config())
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;
        let hop3 = TurnServer::start(make_strict_config())
            .await
            .map_err(|e| Error::InvalidConfig(e.to_string()))?;

        let mut k1 = [0u8; rings_aez::KEY_BYTES];
        let mut k2 = [0u8; rings_aez::KEY_BYTES];
        for i in 0..rings_aez::KEY_BYTES {
            k1[i] = u8::try_from(i + 5).unwrap();
            k2[i] = u8::try_from(i + 61).unwrap();
        }

        let (ice_url, exit_relay_addr, entry_creds) =
            TurnReservationService::reserve_onion_circuit(
                &[&hop1, &hop2, &hop3],
                &[k1, k2],
                Duration::from_secs(60),
            )
            .await?;

        assert_eq!(
            ice_url,
            format!(
                "turn://{}:{}@{}",
                entry_creds.username,
                entry_creds.credential,
                hop1.advertised_addr()
            )
        );
        assert!(
            !entry_creds.username.contains(':'),
            "session-ticket username must be URL-userinfo safe without ':'"
        );
        let parsed_ice: rings_transport::ice_server::IceServer = ice_url
            .parse()
            .map_err(|e| Error::InvalidConfig(format!("failed to parse ICE URL: {e}")))?;
        assert_eq!(parsed_ice.username, entry_creds.username);
        assert_eq!(parsed_ice.credential, entry_creds.credential);
        assert_ne!(exit_relay_addr.port(), 0);

        hop1.close().await.unwrap();
        hop2.close().await.unwrap();
        hop3.close().await.unwrap();
        Ok(())
    }
}
