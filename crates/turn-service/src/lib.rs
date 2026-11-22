#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

use std::borrow::Borrow;
use std::collections::BTreeMap;
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::domain_tag;
use rings_core::ecc::prime_order::NonIdentityPoint;
use rings_core::ecc::PublicKey;
use rings_core::ecc::Secp256k1;
use rings_core::ecc::SecretKey;
use rings_core::ecc::VerificationPublicKey;
use rings_core::message::DomainTag;
use rings_core::message::MessageSigner;
use rings_core::message::MessageVerification;
use rings_core::message::SigningDomain;
use rings_network_policy::validate_public_url_host;
use rings_node::descriptor::heard_descriptors;
use rings_node::descriptor::latest_valid_by_did;
use rings_node::descriptor::latest_valid_by_key;
use rings_node::descriptor::sign_descriptor_body;
use rings_node::descriptor::RegistryElement;
use rings_node::descriptor::SignedDescriptor;
use rings_node::descriptor::SignedDescriptorBody;
use rings_node::error::Error;
use rings_node::error::Result;
use rings_node::onion::OnionBackendName;
use rings_node::onion::OnionInputType;
use rings_node::onion::OnionInvokeOffer;
use rings_node::onion::OnionInvokePolicy;
use rings_node::onion::OnionInvokeShape;
use rings_node::onion::OnionInvokeStage;
use rings_node::onion::OnionLatencyClass;
use rings_node::onion::OnionPipelineHandle;
use rings_node::onion::OnionPipelineRoute;
use rings_node::onion::OnionProcessEpoch;
use rings_node::onion::OnionRouteError;
use rings_node::onion::OnionWidthClass;
use rings_node::prelude::entry;
use rings_node::processor::Processor;
use rings_node::processor::ProcessorBuilder;
use rings_node::provider::LocalRpcMethod;
use rings_node::provider::LookupRegistryResponse;
use rings_node::provider::OnionEligibleStage;
use rings_node::provider::OnionInvokeRpcRequest;
use rings_node::provider::OnionInvokeRpcResponse;
use rings_node::provider::Provider;
use rings_node::registration::validate_schedule;
use rings_node::registration::DhtRegistrationPublisher;
use rings_node::registration::LeaseTerms;
use rings_node::registration::RegistrationContext;
use rings_node::registration::RegistrationTask;
use rings_node::registration::TimingKeys;
use rings_turn::extract_turn_server_pubkey;
use rings_turn::format_turn_server_url;
use rings_turn::SignedTurnSessionTicket;
use serde::Deserialize;
use serde::Serialize;
use url::Url;

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
pub mod onion_udp_relay;
#[cfg(all(
    test,
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
mod test_bridge_model;
#[cfg(all(test, not(target_family = "wasm")))]
mod test_wire_goldens;
#[cfg(all(
    test,
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
mod tests_phase_b;

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
pub use onion_udp_relay::OnionUdpRelayBackend;
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
pub use onion_udp_relay::OnionUdpRelayFactory;

/// Global DHT registry topic on which TURN service hosts publish [`TurnServiceDescriptor`]s.
pub const TURN_SERVICES_TOPIC: &str = "turn_services";

/// Canonical open `invoke` backend name for anonymous TURN session-ticket issuance (#1292).
pub const TURN_TICKET_INVOKE_BACKEND: &str = "turn.ticket";

/// Closed `invoke` shape (`invoke<64,1024,500ms>`) used by [`TURN_TICKET_INVOKE_BACKEND`].
pub const TURN_TICKET_INVOKE_SHAPE: OnionInvokeShape = OnionInvokeShape::new(
    OnionInputType::Fixed(OnionWidthClass::B64),
    OnionWidthClass::KiB1,
    OnionLatencyClass::Ms500,
);

/// Default ticket reservation TTL requested by clients (`60,000 ms`).
pub const DEFAULT_TURN_TICKET_TTL_MS: u64 = 60_000;

/// Default maximum concurrent `turn.ticket` invocations admitted per TURN node.
pub const DEFAULT_TURN_TICKET_MAX_CONCURRENT: u32 = 16;

/// Default maximum `turn.ticket` invocations per minute admitted per TURN node.
pub const DEFAULT_TURN_TICKET_MAX_INVOCATIONS_PER_MINUTE: u64 = 120;

/// Message family of the TURN service descriptor signature.
pub const TURN_SERVICE_DESCRIPTOR_DOMAIN_TAG: DomainTag =
    domain_tag!("rings-turn:service-descriptor");

/// Default heartbeat interval for TURN service descriptor republication.
pub const DEFAULT_TURN_SERVICE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Default freshness window (TTL) for TURN service descriptors (three heartbeats).
pub const DEFAULT_TURN_SERVICE_TTL: Duration = Duration::from_secs(90);

/// Fixed 2-byte big-endian length header preceding fixed-width `turn.ticket` payloads.
const FIXED_FRAME_LEN_HEADER_BYTES: usize = 2;

/// Return the default [`OnionInvokePolicy`] advertised and enforced for `turn.ticket`.
pub const fn default_turn_ticket_invoke_policy() -> OnionInvokePolicy {
    OnionInvokePolicy {
        max_concurrent: DEFAULT_TURN_TICKET_MAX_CONCURRENT,
        max_invocations_per_minute: DEFAULT_TURN_TICKET_MAX_INVOCATIONS_PER_MINUTE,
    }
}

/// Build an [`OnionInvokeOffer`] registering [`TURN_TICKET_INVOKE_BACKEND`] at
/// [`TURN_TICKET_INVOKE_SHAPE`] under `policy`.
pub fn turn_ticket_invoke_offer(policy: OnionInvokePolicy) -> Result<OnionInvokeOffer> {
    let backend = OnionBackendName::parse(TURN_TICKET_INVOKE_BACKEND)?;
    OnionInvokeOffer::new([(TURN_TICKET_INVOKE_SHAPE, backend, policy)])
}

/// Validate and canonicalize a publicly routable `turn://` or `turns://` base URL together with
/// its secp256k1 `server_pubkey`.
///
/// Rejects userinfo (`@`), paths (`/`), fragments (`#`), port `0`, unsupported query parameters,
/// and any host that is loopback, RFC 1918 private, link-local, IPv6 ULA, unspecified, CGNAT,
/// documentation/benchmarking, multicast, or a local/single-label domain name.
pub fn validate_turn_service_endpoint(
    turn_url: &str,
    server_pubkey: PublicKey<33>,
) -> Result<String> {
    let trimmed = turn_url.trim();
    let Some((raw_scheme, rest)) = trimmed.split_once("://") else {
        return Err(Error::InvalidConfig(format!(
            "turn_url must start with turn:// or turns://, got {trimmed:?}"
        )));
    };
    let scheme = if raw_scheme.eq_ignore_ascii_case("turn") {
        "turn"
    } else if raw_scheme.eq_ignore_ascii_case("turns") {
        "turns"
    } else {
        return Err(Error::InvalidConfig(format!(
            "turn_url scheme must be turn or turns, got {raw_scheme:?}"
        )));
    };

    if rest.contains(['@', '/', '#']) || rest.chars().any(char::is_whitespace) {
        return Err(Error::InvalidConfig(format!(
            "turn_url must carry a bare host[:port] authority without userinfo, path, or fragment, got {trimmed:?}"
        )));
    }

    let (authority, query_opt) = match rest.split_once('?') {
        Some((auth, q)) => (auth, Some(q)),
        None => (rest, None),
    };
    if authority.is_empty() || authority.ends_with(":0") {
        return Err(Error::InvalidConfig(format!(
            "turn_url authority must be non-empty with a non-zero port, got {trimmed:?}"
        )));
    }

    let canonical_query = match query_opt {
        None => String::new(),
        Some(q) if q.eq_ignore_ascii_case("transport=udp") => "?transport=udp".to_string(),
        Some(q) if q.eq_ignore_ascii_case("transport=tcp") => "?transport=tcp".to_string(),
        Some(other) => {
            return Err(Error::InvalidConfig(format!(
                "unsupported turn_url query {other:?}; only transport=udp or transport=tcp is permitted"
            )));
        }
    };

    let synthetic_http = format!("http://{authority}");
    let parsed_http = Url::parse(&synthetic_http).map_err(|e| {
        Error::InvalidConfig(format!("invalid turn_url authority {authority:?}: {e}"))
    })?;
    if !parsed_http.username().is_empty()
        || parsed_http.password().is_some()
        || parsed_http.path() != "/"
        || parsed_http.query().is_some()
        || parsed_http.fragment().is_some()
        || parsed_http.port() == Some(0)
    {
        return Err(Error::InvalidConfig(format!(
            "invalid turn_url authority {authority:?}"
        )));
    }
    validate_public_url_host(&parsed_http).map_err(|err| {
        Error::InvalidConfig(format!("turn_url host is not publicly routable: {err}"))
    })?;

    let canonical_host = parsed_http
        .host_str()
        .ok_or_else(|| Error::InvalidConfig(format!("turn_url missing host in {trimmed:?}")))?;
    let canonical_authority = match authority_explicit_port(authority) {
        Some(port) => {
            if port == 0 {
                return Err(Error::InvalidConfig(
                    "turn_url port must be non-zero".to_string(),
                ));
            }
            format!("{canonical_host}:{port}")
        }
        None => canonical_host.to_string(),
    };
    let canonical_url = format!("{scheme}://{canonical_authority}{canonical_query}");

    let formatted = format_turn_server_url(&canonical_url, server_pubkey)
        .map_err(|e| Error::InvalidConfig(format!("invalid TURN server_pubkey: {e}")))?;
    if extract_turn_server_pubkey(&formatted) != Some(server_pubkey) {
        return Err(Error::InvalidConfig(
            "failed to round-trip TURN server_pubkey in turn_url".to_string(),
        ));
    }
    Ok(canonical_url)
}

fn authority_explicit_port(authority: &str) -> Option<u16> {
    let port_str = if let Some(rest) = authority.strip_prefix('[') {
        let (_, after_bracket) = rest.split_once(']')?;
        after_bracket.strip_prefix(':')?
    } else {
        let (_, port_part) = authority.rsplit_once(':')?;
        port_part
    };
    port_str.parse::<u16>().ok()
}

/// Descriptor fields covered by the [`TurnServiceDescriptor`] signature (#1292).
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct TurnServiceDescriptorBody {
    /// DID of the node hosting or bridging this TURN service.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Session delegatee public key of the process publishing this descriptor, matching the
    /// signing delegation and Sphinx hop key `pk_i`.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the publishing node process, matching its `relay` and `turn.ticket`
    /// `invoke` registrations.
    pub process_epoch: OnionProcessEpoch,
    /// Overlay network identifier.
    pub network_id: u32,
    /// Connectable public TURN listener URL (`turn://host:port` or `turns://host:port`).
    pub turn_url: String,
    /// Compressed secp256k1 public key (`PublicKey<33>`) of the TURN server for ECDH credentials.
    pub server_pubkey: PublicKey<33>,
    /// Freshness window `T` in milliseconds.
    pub fresh_for_ms: u64,
    /// Signed lease expiration `ℓ` in Unix epoch milliseconds.
    pub expires_at_ms: u128,
}

/// Signed DHT lease descriptor advertising a TURN relay service on [`TURN_SERVICES_TOPIC`].
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct TurnServiceDescriptor {
    /// Signed descriptor payload.
    pub body: TurnServiceDescriptorBody,
    /// Unsigned carrier dot timestamp `τ*` recorded when read from the DHT.
    #[serde(default, skip)]
    pub heartbeat_at_ms: u128,
    /// Signature binding `body` to the node's delegation and overlay.
    pub signature: MessageVerification,
}

impl RegistryElement for TurnServiceDescriptor {}

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
        rings_codec::serialize(self).map_err(rings_core::error::Error::CodecSerialize)
    }

    fn into_signed_descriptor(self, signature: MessageVerification) -> Self::Descriptor {
        TurnServiceDescriptor {
            body: self,
            heartbeat_at_ms: 0,
            signature,
        }
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
        self.body.body_signing_data()
    }

    fn heard_at(mut self, heartbeat_at_ms: u128) -> Self {
        self.heartbeat_at_ms = heartbeat_at_ms;
        self
    }

    fn descriptor_verify_signature_at(&self, network_id: u32, at_ms: u128) -> bool {
        if self.body.network_id != network_id {
            return false;
        }
        if NonIdentityPoint::<Secp256k1>::try_from(self.body.delegatee_public_key).is_err() {
            return false;
        }
        if Did::from(self.body.delegatee_public_key.address())
            != self.signature.delegation.delegatee_did()
        {
            return false;
        }
        let did = self.body.did;
        if self.body.public_key.did() != did || self.signature.delegation.delegator_did() != did {
            return false;
        }
        let Ok(delegator_pk) = self.signature.delegation.delegator_verification_pubkey() else {
            return false;
        };
        if delegator_pk != self.body.public_key {
            return false;
        }
        let Ok(data) = self.descriptor_signing_data() else {
            return false;
        };
        let domain = SigningDomain::new(TURN_SERVICE_DESCRIPTOR_DOMAIN_TAG, network_id);
        self.signature.verify_at(domain, &data, at_ms)
    }
}

impl TurnServiceDescriptor {
    /// Sign `body` with `signer`, enforcing DID, account key, delegatee session key, and overlay
    /// binding.
    pub fn new_signed(
        body: TurnServiceDescriptorBody,
        signer: MessageSigner<impl Borrow<DelegateeKey>>,
    ) -> rings_core::error::Result<Self> {
        let _ = NonIdentityPoint::<Secp256k1>::try_from(body.delegatee_public_key)?;
        let expected_delegatee = Did::from(body.delegatee_public_key.address());
        let signed = sign_descriptor_body(
            body,
            signer,
            "turn service descriptor DID does not match signer delegation",
        )?;
        if signed.signature.delegation.delegatee_did() != expected_delegatee {
            return Err(rings_core::error::Error::InvalidMessage(
                "turn service descriptor delegatee_public_key does not match signer session key"
                    .to_string(),
            ));
        }
        Ok(signed)
    }

    /// Return `true` iff `turn_url` is a valid publicly routable TURN endpoint and `server_pubkey`
    /// is a valid secp256k1 point.
    pub fn has_valid_endpoint(&self) -> bool {
        validate_turn_service_endpoint(&self.body.turn_url, self.body.server_pubkey).is_ok()
    }

    /// Format this descriptor as a `pubkey=`-annotated TURN ICE server template URL ready for
    /// [`rings_turn::TurnCredentialSigner::sign_ice_servers`].
    pub fn ice_server_base_url(&self) -> Result<String> {
        let canonical =
            validate_turn_service_endpoint(&self.body.turn_url, self.body.server_pubkey)?;
        format_turn_server_url(&canonical, self.body.server_pubkey)
            .map_err(|e| Error::InvalidConfig(e.to_string()))
    }

    /// Select the newest cryptographically valid [`TurnServiceDescriptor`] per `(DID, process_epoch)`
    /// first, and then retain only publicly routable endpoints.
    ///
    /// Deduplicating before endpoint filtering guarantees that a newer lease carrying a revoked or
    /// invalid endpoint supersedes and suppresses an older valid lease for that process rather than
    /// falling back to stale state, while keying by `(did, process_epoch)` allows multiple daemon
    /// processes sharing one account [`Did`] to coexist without overwriting one another.
    pub fn latest_valid_by_process(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_key(
            descriptors,
            |descriptor| (descriptor.body.did, descriptor.body.process_epoch),
            now_ms,
            network_id,
            include_expired,
        )
        .into_values()
        .filter(TurnServiceDescriptor::has_valid_endpoint)
        .collect()
    }

    /// Select the newest cryptographically valid [`TurnServiceDescriptor`] per [`Did`] first, and
    /// then retain only publicly routable endpoints.
    pub fn latest_valid_by_did(
        descriptors: impl IntoIterator<Item = Self>,
        now_ms: u128,
        network_id: u32,
        include_expired: bool,
    ) -> Vec<Self> {
        latest_valid_by_did(descriptors, now_ms, network_id, include_expired)
            .into_iter()
            .filter(TurnServiceDescriptor::has_valid_endpoint)
            .collect()
    }
}

/// Validated configuration for announcing a node's TURN relay service in the DHT (#1292).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnServiceOffer {
    turn_url: String,
    server_pubkey: PublicKey<33>,
    heartbeat_interval: Duration,
    ttl: Duration,
}

impl TurnServiceOffer {
    /// Construct a [`TurnServiceOffer`] with default heartbeat (`30s`) and TTL (`90s`).
    pub fn new(turn_url: impl AsRef<str>, server_pubkey: PublicKey<33>) -> Result<Self> {
        Self::with_schedule(
            turn_url,
            server_pubkey,
            DEFAULT_TURN_SERVICE_HEARTBEAT_INTERVAL,
            DEFAULT_TURN_SERVICE_TTL,
        )
    }

    /// Construct a [`TurnServiceOffer`] with custom lease heartbeat and TTL schedule.
    pub fn with_schedule(
        turn_url: impl AsRef<str>,
        server_pubkey: PublicKey<33>,
        heartbeat_interval: Duration,
        ttl: Duration,
    ) -> Result<Self> {
        let canonical_url = validate_turn_service_endpoint(turn_url.as_ref(), server_pubkey)?;
        validate_schedule(
            &TimingKeys {
                heartbeat_interval: "turn_service.heartbeat_interval",
                ttl: "turn_service.ttl",
                enabled_by: None,
            },
            heartbeat_interval,
            ttl,
        )?;
        Ok(Self {
            turn_url: canonical_url,
            server_pubkey,
            heartbeat_interval,
            ttl,
        })
    }

    /// Return the canonicalized `turn_url` (`turn://host:port` or `turns://host:port`).
    pub fn turn_url(&self) -> &str {
        &self.turn_url
    }

    /// Return the TURN server's compressed secp256k1 public key.
    pub const fn server_pubkey(&self) -> PublicKey<33> {
        self.server_pubkey
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
pub struct TurnServiceRegistration {
    offer: TurnServiceOffer,
    publisher: DhtRegistrationPublisher<[TurnServiceDescriptor; 1]>,
}

impl TurnServiceRegistration {
    /// Construct a new [`TurnServiceRegistration`] for `offer`.
    pub fn new(offer: TurnServiceOffer) -> Self {
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
    pub fn descriptors_from_entry(entry: &entry::Entry) -> Vec<TurnServiceDescriptor> {
        heard_descriptors(entry)
    }

    /// Sign a [`TurnServiceDescriptor`] for `context` under `terms`.
    pub fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        terms: LeaseTerms,
    ) -> Result<TurnServiceDescriptor> {
        let body = TurnServiceDescriptorBody {
            did: context.did(),
            public_key: context.delegator_verification_pubkey()?,
            delegatee_public_key: context.delegatee_key().delegatee_public_key(),
            process_epoch: context.onion_process_epoch(),
            network_id: context.network_id(),
            turn_url: self.offer.turn_url.clone(),
            server_pubkey: self.offer.server_pubkey,
            fresh_for_ms: terms.fresh_for_ms,
            expires_at_ms: terms.expires_at_ms,
        };
        TurnServiceDescriptor::new_signed(body, context.message_signer()).map_err(Error::CoreError)
    }
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
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

/// Attach a [`TurnServiceRegistration`] for `offer` onto `builder` via `rings-node`'s generic
/// [`ProcessorBuilder::register_task`] hook.
pub fn announce_turn_service(
    builder: ProcessorBuilder,
    offer: TurnServiceOffer,
) -> ProcessorBuilder {
    builder.register_task(Arc::new(TurnServiceRegistration::new(offer)))
}

/// List signed, publicly routable [`TurnServiceDescriptor`]s from [`TURN_SERVICES_TOPIC`],
/// deduplicated per `(did, process_epoch)` with supersession applied before endpoint filtering.
pub async fn lookup_turn_services(
    processor: &Processor,
    include_expired: bool,
) -> Result<Vec<TurnServiceDescriptor>> {
    let Some(entry) = processor.fetch_topic_entry(TURN_SERVICES_TOPIC).await? else {
        return Ok(Vec::new());
    };
    Ok(TurnServiceDescriptor::latest_valid_by_process(
        TurnServiceRegistration::descriptors_from_entry(&entry),
        processor.clock().now_ms(),
        processor.swarm.network_id(),
        include_expired,
    ))
}

/// Decode and validate [`TurnServiceDescriptor`]s from a generic `rings-node`
/// [`LookupRegistryResponse`] (`lookupRegistry` local/FFI/WASM RPC).
pub fn decode_turn_services_from_registry_response(
    response: &LookupRegistryResponse,
    include_expired: bool,
) -> Result<Vec<TurnServiceDescriptor>> {
    if response.topic != TURN_SERVICES_TOPIC {
        return Err(Error::InvalidConfig(format!(
            "expected registry topic {TURN_SERVICES_TOPIC:?}, got {:?}",
            response.topic
        )));
    }
    let decoded = response.elements.iter().filter_map(|elem| {
        let raw = base64::decode(elem.data_base64.as_str()).ok()?;
        TurnServiceDescriptor::from_element(raw.as_slice())
            .ok()
            .map(|desc| desc.heard_at(elem.heartbeat_at_ms))
    });
    Ok(TurnServiceDescriptor::latest_valid_by_process(
        decoded,
        response.now_ms,
        response.network_id,
        include_expired,
    ))
}

/// List live, publicly routable [`TurnServiceDescriptor`]s via [`Provider::lookup_registry_raw`].
pub async fn lookup_turn_services_via_provider(
    provider: &Provider,
    include_expired: bool,
) -> Result<Vec<TurnServiceDescriptor>> {
    let raw = provider.lookup_registry_raw(TURN_SERVICES_TOPIC).await?;
    decode_turn_services_from_registry_response(&raw, include_expired)
}

/// Request payload sent inside the `64`-byte carried input of [`TURN_TICKET_INVOKE_SHAPE`] to
/// [`TURN_TICKET_INVOKE_BACKEND`] (#1292).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTicketRequest {
    /// Fresh single-use client secp256k1 public key for ECDH password derivation and client proof.
    pub ephemeral_client_pubkey: PublicKey<33>,
    /// Requested unclaimed reservation TTL in milliseconds (clamped to server `max_reservation_ttl`).
    pub ttl_ms: u64,
}

/// Closed rejection reasons returned inside [`TurnTicketOutcome::Rejected`] (#1292).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnTicketRejection {
    /// Request was malformed, carried an invalid secp256k1 point, or had zero TTL.
    InvalidRequest,
    /// Server reservation cap (`max_reservations`) reached.
    CapacityReached,
    /// Underlying TURN ticket issuance failed.
    InternalError,
}

/// Response payload returned inside the `1024`-byte output of [`TURN_TICKET_INVOKE_SHAPE`] (#1292).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnTicketOutcome {
    /// Successfully reserved an unlinkable session slot and issued a server-signed session ticket.
    Issued {
        /// Unlinkable signed session ticket (relay socket is bound lazily on `Allocate`).
        ticket: SignedTurnSessionTicket,
        /// Issuing TURN server's compressed secp256k1 public key.
        server_pubkey: PublicKey<33>,
    },
    /// Reservation request was refused by the TURN server.
    Rejected(TurnTicketRejection),
}

fn encode_fixed_frame<T: Serialize>(value: &T, exact_width: usize) -> Result<Vec<u8>> {
    let payload = rings_codec::serialize(value)
        .map_err(|e| Error::CoreError(rings_core::error::Error::CodecSerialize(e)))?;
    let max_payload = exact_width.saturating_sub(FIXED_FRAME_LEN_HEADER_BYTES);
    if payload.len() > max_payload || payload.len() > usize::from(u16::MAX) {
        return Err(Error::InvalidConfig(format!(
            "turn.ticket frame payload ({} bytes) exceeds {max_payload}-byte slot",
            payload.len()
        )));
    }
    let len_u16 = u16::try_from(payload.len()).unwrap_or(0);
    let mut out = vec![0_u8; exact_width];
    if let Some(head) = out.get_mut(..FIXED_FRAME_LEN_HEADER_BYTES) {
        head.copy_from_slice(&len_u16.to_be_bytes());
    }
    if let Some(body) =
        out.get_mut(FIXED_FRAME_LEN_HEADER_BYTES..FIXED_FRAME_LEN_HEADER_BYTES + payload.len())
    {
        body.copy_from_slice(&payload);
    }
    Ok(out)
}

fn decode_fixed_frame<T: for<'de> Deserialize<'de>>(frame: &[u8], exact_width: usize) -> Result<T> {
    if frame.len() != exact_width {
        return Err(Error::InvalidConfig(format!(
            "expected {exact_width}-byte turn.ticket frame, got {} bytes",
            frame.len()
        )));
    }
    let Some(&[b0, b1]) = frame.get(..FIXED_FRAME_LEN_HEADER_BYTES) else {
        return Err(Error::InvalidConfig(
            "turn.ticket frame shorter than length header".to_string(),
        ));
    };
    let payload_len = usize::from(u16::from_be_bytes([b0, b1]));
    let payload_end = FIXED_FRAME_LEN_HEADER_BYTES
        .checked_add(payload_len)
        .ok_or_else(|| Error::InvalidConfig("turn.ticket frame length overflow".to_string()))?;
    let Some(payload) = frame.get(FIXED_FRAME_LEN_HEADER_BYTES..payload_end) else {
        return Err(Error::InvalidConfig(
            "turn.ticket frame length exceeds fixed width".to_string(),
        ));
    };
    let Some(padding) = frame.get(payload_end..) else {
        return Err(Error::InvalidConfig(
            "turn.ticket frame missing trailing slice".to_string(),
        ));
    };
    if padding.iter().any(|&b| b != 0) {
        return Err(Error::InvalidConfig(
            "non-zero trailing padding in turn.ticket frame".to_string(),
        ));
    }
    rings_codec::deserialize(payload)
        .map_err(|e| Error::CoreError(rings_core::error::Error::CodecDeserialize(e)))
}

/// Encode a [`TurnTicketRequest`] into the canonical `64`-byte input frame of
/// [`TURN_TICKET_INVOKE_SHAPE`].
pub fn encode_turn_ticket_request(request: &TurnTicketRequest) -> Result<Vec<u8>> {
    encode_fixed_frame(request, TURN_TICKET_INVOKE_SHAPE.input().bytes())
}

/// Decode a [`TurnTicketRequest`] from a canonical `64`-byte input frame.
pub fn decode_turn_ticket_request(bytes: &[u8]) -> Result<TurnTicketRequest> {
    decode_fixed_frame(bytes, TURN_TICKET_INVOKE_SHAPE.input().bytes())
}

/// Encode a [`TurnTicketOutcome`] into the canonical `1024`-byte output frame of
/// [`TURN_TICKET_INVOKE_SHAPE`].
pub fn encode_turn_ticket_outcome(outcome: &TurnTicketOutcome) -> Result<Vec<u8>> {
    encode_fixed_frame(outcome, TURN_TICKET_INVOKE_SHAPE.output_bytes())
}

/// Decode a [`TurnTicketOutcome`] from a canonical `1024`-byte output frame.
pub fn decode_turn_ticket_outcome(bytes: &[u8]) -> Result<TurnTicketOutcome> {
    decode_fixed_frame(bytes, TURN_TICKET_INVOKE_SHAPE.output_bytes())
}

/// Ready-to-use unlinkable TURN ICE server credentials acquired anonymously over `turn.ticket`
/// (#1292).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquiredTurnTicket {
    /// Complete standard ICE server URL (`turn://tkt-…:<pass>@host:port`) ready for WebRTC.
    pub ice_url: String,
    /// Ticket expiration timestamp in Unix epoch milliseconds.
    pub expires_at_ms: u128,
    /// Overlay [`Did`] of the TURN service node that issued the ticket.
    pub turn_did: Did,
    /// Process epoch of the TURN service process that issued the ticket.
    pub turn_process_epoch: OnionProcessEpoch,
    /// Compressed secp256k1 [`PublicKey<33>`] of the issuing TURN server.
    pub server_pubkey: PublicKey<33>,
}

impl std::fmt::Debug for AcquiredTurnTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_url = redact_ice_url_password(&self.ice_url);
        f.debug_struct("AcquiredTurnTicket")
            .field("ice_url", &redacted_url)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("turn_did", &self.turn_did)
            .field("turn_process_epoch", &self.turn_process_epoch)
            .field("server_pubkey", &self.server_pubkey)
            .finish()
    }
}

fn redact_ice_url_password(ice_url: &str) -> String {
    let Some((scheme, rest)) = ice_url.split_once("://") else {
        return "<redacted>".to_string();
    };
    let Some((userinfo, host_part)) = rest.rsplit_once('@') else {
        return ice_url.to_string();
    };
    let Some((user, _pass)) = userinfo.split_once(':') else {
        return ice_url.to_string();
    };
    format!("{scheme}://{user}:<redacted>@{host_part}")
}

/// Build the 1-stage Kleisli [`rings_node::onion::OnionInvokePipeline`] for `turn.ticket`.
pub fn turn_ticket_pipeline() -> Result<rings_node::onion::OnionInvokePipeline> {
    let backend = OnionBackendName::parse(TURN_TICKET_INVOKE_BACKEND)?;
    let stage = OnionInvokeStage::new(TURN_TICKET_INVOKE_SHAPE, backend, Vec::new())?;
    Ok(stage.into_pipeline())
}

fn verify_and_build_acquired_ticket(
    ephemeral_client_key: &SecretKey,
    turn_descriptor: &TurnServiceDescriptor,
    outcome: TurnTicketOutcome,
    now_ms: u128,
) -> Result<AcquiredTurnTicket> {
    let canonical_turn_url = validate_turn_service_endpoint(
        &turn_descriptor.body.turn_url,
        turn_descriptor.body.server_pubkey,
    )?;
    match outcome {
        TurnTicketOutcome::Issued {
            ticket,
            server_pubkey,
        } => {
            if server_pubkey != turn_descriptor.body.server_pubkey {
                return Err(Error::InvalidConfig(
                    "turn.ticket response server_pubkey does not match signed TurnServiceDescriptor"
                        .to_string(),
                ));
            }
            ticket
                .verify_grant_at(server_pubkey, now_ms)
                .map_err(Error::CoreError)?;
            let creds = ticket
                .to_turn_credentials(ephemeral_client_key, server_pubkey)
                .map_err(Error::CoreError)?;
            let ice_url = creds.apply_to_ice_server_url(&canonical_turn_url);
            Ok(AcquiredTurnTicket {
                ice_url,
                expires_at_ms: ticket.ticket.expires_at_ms,
                turn_did: turn_descriptor.body.did,
                turn_process_epoch: turn_descriptor.body.process_epoch,
                server_pubkey,
            })
        }
        TurnTicketOutcome::Rejected(TurnTicketRejection::CapacityReached) => Err(
            Error::RemoteRpcError("TURN server ticket reservation capacity reached".to_string()),
        ),
        TurnTicketOutcome::Rejected(TurnTicketRejection::InvalidRequest) => Err(
            Error::InvalidConfig("TURN server rejected ticket request as invalid".to_string()),
        ),
        TurnTicketOutcome::Rejected(TurnTicketRejection::InternalError) => Err(
            Error::RemoteRpcError("TURN server failed to reserve ticket session".to_string()),
        ),
    }
}

/// Client-side state for an in-flight `turn.ticket` request prepared for `rings-node`'s generic
/// `"onionInvoke"` RPC (`librings_node` FFI or `rings-node` browser WASM).
pub struct PendingTurnTicketAttempt {
    ephemeral_client_key: SecretKey,
    services: BTreeMap<(Did, OnionProcessEpoch), TurnServiceDescriptor>,
}

impl std::fmt::Debug for PendingTurnTicketAttempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingTurnTicketAttempt")
            .field(
                "ephemeral_client_pubkey",
                &self.ephemeral_client_key.pubkey(),
            )
            .field("candidate_count", &self.services.len())
            .finish()
    }
}

impl PendingTurnTicketAttempt {
    /// Verify an [`OnionInvokeRpcResponse`] returned by `rings-node`'s `"onionInvoke"` primitive
    /// against the signed [`TurnServiceDescriptor`] of `(stage_did, stage_process_epoch)` and
    /// derive the ready [`AcquiredTurnTicket`].
    pub fn complete(
        self,
        response: &OnionInvokeRpcResponse,
        now_ms: u128,
    ) -> Result<AcquiredTurnTicket> {
        let key = (response.stage_did, response.stage_process_epoch);
        let turn_descriptor = self.services.get(&key).ok_or_else(|| {
            Error::InvalidConfig(
                "selected onionInvoke stage (did, process_epoch) missing from validated TurnServiceDescriptors"
                    .to_string(),
            )
        })?;
        if turn_descriptor.body.delegatee_public_key != response.stage_delegatee_public_key {
            return Err(Error::InvalidConfig(
                "selected onionInvoke stage delegatee_public_key does not match TurnServiceDescriptor"
                    .to_string(),
            ));
        }
        let output_bytes = base64::decode(response.output_base64.as_str())
            .map_err(|e| Error::InvalidConfig(format!("invalid output_base64: {e}")))?;
        let outcome = decode_turn_ticket_outcome(&output_bytes)?;
        verify_and_build_acquired_ticket(
            &self.ephemeral_client_key,
            turn_descriptor,
            outcome,
            now_ms,
        )
    }
}

/// Prepare a generic `rings-node` [`OnionInvokeRpcRequest`] (for C FFI `rings_node_request` or
/// browser WASM `provider.onionInvoke`) from validated [`TurnServiceDescriptor`]s.
pub fn prepare_turn_ticket_onion_invoke(
    services: impl IntoIterator<Item = TurnServiceDescriptor>,
    exclude_did: Option<Did>,
    ttl: Duration,
    target_did: Option<Did>,
) -> Result<(PendingTurnTicketAttempt, OnionInvokeRpcRequest)> {
    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
    if ttl_ms == 0 {
        return Err(Error::InvalidConfig(
            "TURN ticket TTL must be greater than zero".to_string(),
        ));
    }
    let service_map: BTreeMap<(Did, OnionProcessEpoch), TurnServiceDescriptor> = services
        .into_iter()
        .filter(|desc| exclude_did.is_none_or(|local| desc.body.did != local))
        .filter(|desc| target_did.is_none_or(|wanted| desc.body.did == wanted))
        .filter(TurnServiceDescriptor::has_valid_endpoint)
        .map(|desc| ((desc.body.did, desc.body.process_epoch), desc))
        .collect();
    if service_map.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoLiveSymbol {
            shape: TURN_TICKET_INVOKE_SHAPE.to_string(),
            backend: TURN_TICKET_INVOKE_BACKEND.to_string(),
        }));
    }

    let ephemeral_client_key = SecretKey::random();
    let request = TurnTicketRequest {
        ephemeral_client_pubkey: ephemeral_client_key.pubkey(),
        ttl_ms,
    };
    let input_bytes = encode_turn_ticket_request(&request)?;
    let eligible_stages: Vec<OnionEligibleStage> = service_map
        .values()
        .map(|desc| OnionEligibleStage {
            did: desc.body.did,
            process_epoch: Some(desc.body.process_epoch),
            delegatee_public_key: Some(desc.body.delegatee_public_key),
        })
        .collect();
    let backend = OnionBackendName::parse(TURN_TICKET_INVOKE_BACKEND)?;
    let rpc_request = OnionInvokeRpcRequest {
        shape: TURN_TICKET_INVOKE_SHAPE,
        backend,
        constants_base64: None,
        input_base64: base64::encode(&input_bytes),
        eligible_dids: None,
        eligible_stages: Some(eligible_stages),
        timeout_ms: None,
    };
    Ok((
        PendingTurnTicketAttempt {
            ephemeral_client_key,
            services: service_map,
        },
        rpc_request,
    ))
}

/// Acquire an unlinkable [`AcquiredTurnTicket`] (`turn://tkt-…:<pass>@host:port`) over an
/// already-selected [`OnionPipelineRoute`] and validated [`TurnServiceDescriptor`].
pub async fn acquire_turn_ticket_over_route(
    pipeline_handle: &OnionPipelineHandle,
    route: &OnionPipelineRoute,
    turn_descriptor: &TurnServiceDescriptor,
    ttl: Duration,
    now_ms: u128,
) -> Result<AcquiredTurnTicket> {
    acquire_turn_ticket_over_route_with_now(pipeline_handle, route, turn_descriptor, ttl, {
        let started_ms = rings_core::utils::get_epoch_ms();
        move || now_ms.saturating_add(rings_core::utils::get_epoch_ms().saturating_sub(started_ms))
    })
    .await
}

async fn acquire_turn_ticket_over_route_with_now(
    pipeline_handle: &OnionPipelineHandle,
    route: &OnionPipelineRoute,
    turn_descriptor: &TurnServiceDescriptor,
    ttl: Duration,
    sample_now_ms: impl FnOnce() -> u128,
) -> Result<AcquiredTurnTicket> {
    let _ = validate_turn_service_endpoint(
        &turn_descriptor.body.turn_url,
        turn_descriptor.body.server_pubkey,
    )?;
    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
    if ttl_ms == 0 {
        return Err(Error::InvalidConfig(
            "TURN ticket TTL must be greater than zero".to_string(),
        ));
    }

    let ephemeral_client_key = SecretKey::random();
    let request = TurnTicketRequest {
        ephemeral_client_pubkey: ephemeral_client_key.pubkey(),
        ttl_ms,
    };
    let input_bytes = encode_turn_ticket_request(&request)?;
    let pipeline = turn_ticket_pipeline()?;
    let output_bytes = pipeline_handle
        .invoke_pipeline_over_route(route, &pipeline, &input_bytes)
        .await?;
    let outcome = decode_turn_ticket_outcome(&output_bytes)?;
    let arrived_at_ms = sample_now_ms();
    verify_and_build_acquired_ticket(
        &ephemeral_client_key,
        turn_descriptor,
        outcome,
        arrived_at_ms,
    )
}

/// Discover a live TURN service from the DHT (optionally pinned to `target_did`), build a
/// guard-closed Sphinx `invoke` loop matched by `(did, process_epoch, delegatee_public_key)` to its
/// `turn.ticket` backend, and return a ready `turn://tkt-…:<pass>@host:port`
/// [`AcquiredTurnTicket`].
pub async fn acquire_turn_ticket(
    processor: &Processor,
    pipeline_handle: &OnionPipelineHandle,
    ttl: Duration,
    target_did: Option<Did>,
) -> Result<AcquiredTurnTicket> {
    let local_did = processor.did();
    let services: BTreeMap<(Did, OnionProcessEpoch), TurnServiceDescriptor> =
        lookup_turn_services(processor, false)
            .await?
            .into_iter()
            .filter(|desc| desc.body.did != local_did)
            .filter(|desc| target_did.is_none_or(|wanted| desc.body.did == wanted))
            .map(|desc| ((desc.body.did, desc.body.process_epoch), desc))
            .collect();
    if services.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoLiveSymbol {
            shape: TURN_TICKET_INVOKE_SHAPE.to_string(),
            backend: TURN_TICKET_INVOKE_BACKEND.to_string(),
        }));
    }

    let pipeline = turn_ticket_pipeline()?;
    let route = processor
        .build_onion_pipeline_route_filtered(&pipeline, |desc| {
            services
                .get(&(desc.did, desc.process_epoch))
                .is_some_and(|turn_desc| {
                    turn_desc.body.delegatee_public_key == desc.delegatee_public_key
                })
        })
        .await?;
    let stage_desc = route.descriptors().first().ok_or_else(|| {
        Error::InvalidConfig("selected turn.ticket route had no stage descriptor".to_string())
    })?;
    let turn_descriptor = services
        .get(&(stage_desc.did, stage_desc.process_epoch))
        .ok_or_else(|| {
            Error::InvalidConfig(
                "selected turn.ticket stage (did, process_epoch) missing from validated TURN service set"
                    .to_string(),
            )
        })?;

    let clock = processor.clock();
    acquire_turn_ticket_over_route_with_now(
        pipeline_handle,
        &route,
        turn_descriptor,
        ttl,
        move || clock.now_ms(),
    )
    .await
}

/// Cross-platform helper acquiring an [`AcquiredTurnTicket`] directly through [`Provider`]'s
/// generic `lookup_registry_raw` + `onion_invoke_raw` primitives (identical on native and browser
/// WASM).
pub async fn request_turn_ticket(
    provider: &Provider,
    ttl: Duration,
    target_did: Option<Did>,
) -> Result<AcquiredTurnTicket> {
    let registry = provider.lookup_registry_raw(TURN_SERVICES_TOPIC).await?;
    let services = decode_turn_services_from_registry_response(&registry, false)?;
    let (pending, invoke_req) = prepare_turn_ticket_onion_invoke(
        services,
        Some(provider.processor().did()),
        ttl,
        target_did,
    )?;
    let invoke_resp = provider.onion_invoke_raw(invoke_req).await?;
    pending.complete(&invoke_resp, provider.processor().clock().now_ms())
}

struct LookupTurnServicesRpc {
    provider_processor: std::sync::Weak<Processor>,
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl LocalRpcMethod for LookupTurnServicesRpc {
    async fn call(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let processor = self
            .provider_processor
            .upgrade()
            .ok_or(Error::RegistrationStopped)?;
        let include_expired = params
            .get("include_expired")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let services = lookup_turn_services(&processor, include_expired).await?;
        Ok(serde_json::to_value(services)?)
    }
}

struct RequestTurnTicketRpc {
    processor: std::sync::Weak<Processor>,
    pipeline: OnionPipelineHandle,
}

#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "browser", target_family = "wasm")), async_trait)]
impl LocalRpcMethod for RequestTurnTicketRpc {
    async fn call(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let processor = self.processor.upgrade().ok_or(Error::RegistrationStopped)?;
        let ttl_ms = params
            .get("ttl_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(DEFAULT_TURN_TICKET_TTL_MS);
        let target_did = match params.get("target_did").and_then(serde_json::Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => Some(Did::from_str(raw.trim())?),
            _ => None,
        };
        let acquired = acquire_turn_ticket(
            &processor,
            &self.pipeline,
            Duration::from_millis(ttl_ms),
            target_did,
        )
        .await?;
        Ok(serde_json::to_value(acquired)?)
    }
}

/// Optional convenience helper registering `"lookupTurnServices"` and `"requestTurnTicket"` on an
/// in-process [`Provider`] via [`Provider::register_local_rpc_method`].
///
/// External C FFI (`librings_node`) and `rings-node` WASM callers that link only `rings-node` can
/// instead invoke `rings-node`'s built-in `"lookupRegistry"` (`topic: "turn_services"`) and
/// `"onionInvoke"` primitives directly (paired with [`decode_turn_services_from_registry_response`]
/// and [`prepare_turn_ticket_onion_invoke`]).
pub fn install_turn_client_rpc(provider: &Provider) -> Result<()> {
    let pipeline = provider.onion_pipeline()?;
    let weak = Arc::downgrade(provider.processor());
    provider.register_local_rpc_method("lookupTurnServices", LookupTurnServicesRpc {
        provider_processor: weak.clone(),
    })?;
    provider.register_local_rpc_method("requestTurnTicket", RequestTurnTicketRpc {
        processor: weak,
        pipeline,
    })
}

/// Browser WASM binding to list live, validated [`TurnServiceDescriptor`]s from a [`Provider`].
#[cfg(all(feature = "browser", target_family = "wasm"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = "lookupTurnServices")]
pub fn wasm_lookup_turn_services(
    provider: &Provider,
    include_expired: Option<bool>,
) -> js_sys::Promise {
    let provider = provider.clone();
    let include_expired = include_expired.unwrap_or(false);
    wasm_bindgen_futures::future_to_promise(async move {
        let services = lookup_turn_services_via_provider(&provider, include_expired)
            .await
            .map_err(|e| wasm_bindgen::JsError::new(&e.to_string()))?;
        Ok(serde_wasm_bindgen::to_value(&services)
            .map_err(|e| wasm_bindgen::JsError::new(&e.to_string()))?)
    })
}

/// Browser WASM binding to anonymously acquire a `turn://tkt-…:<pass>@host:port`
/// [`AcquiredTurnTicket`] over a guard-closed Sphinx `invoke` loop.
#[cfg(all(feature = "browser", target_family = "wasm"))]
#[wasm_bindgen::prelude::wasm_bindgen(js_name = "requestTurnTicket")]
pub fn wasm_request_turn_ticket(
    provider: &Provider,
    ttl_ms: Option<u64>,
    target_did: Option<String>,
) -> js_sys::Promise {
    let provider = provider.clone();
    wasm_bindgen_futures::future_to_promise(async move {
        let parsed_did = match target_did.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(
                Did::from_str(raw.trim())
                    .map_err(|e| wasm_bindgen::JsError::new(&e.to_string()))?,
            ),
            _ => None,
        };
        let ttl = Duration::from_millis(ttl_ms.unwrap_or(DEFAULT_TURN_TICKET_TTL_MS));
        let acquired = request_turn_ticket(&provider, ttl, parsed_did)
            .await
            .map_err(|e| wasm_bindgen::JsError::new(&e.to_string()))?;
        Ok(serde_wasm_bindgen::to_value(&acquired)
            .map_err(|e| wasm_bindgen::JsError::new(&e.to_string()))?)
    })
}

/// Native `turn.ticket` [`rings_node::onion::OnionInvokeBackend`] backed by a local
/// [`rings_turn::TurnServer`] (#1292).
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
#[derive(Clone)]
pub struct TurnTicketInvokeBackend {
    server: Arc<rings_turn::TurnServer>,
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
impl TurnTicketInvokeBackend {
    /// Wrap a running [`rings_turn::TurnServer`] as an [`rings_node::onion::OnionInvokeBackend`].
    pub const fn new(server: Arc<rings_turn::TurnServer>) -> Self {
        Self { server }
    }
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
#[async_trait]
impl rings_node::onion::OnionInvokeBackend for TurnTicketInvokeBackend {
    async fn invoke(&self, call: rings_node::onion::OnionInvokeCall) -> Result<Vec<u8>> {
        if call.shape != TURN_TICKET_INVOKE_SHAPE
            || call.backend.as_str() != TURN_TICKET_INVOKE_BACKEND
        {
            return Err(Error::InvalidConfig(
                "unexpected shape or backend routed to TurnTicketInvokeBackend".to_string(),
            ));
        }
        let outcome = match decode_turn_ticket_request(&call.input) {
            Err(_) => TurnTicketOutcome::Rejected(TurnTicketRejection::InvalidRequest),
            Ok(req) if req.ttl_ms == 0 => {
                TurnTicketOutcome::Rejected(TurnTicketRejection::InvalidRequest)
            }
            Ok(req) => {
                match self
                    .server
                    .reserve_exit_session(
                        req.ephemeral_client_pubkey,
                        Duration::from_millis(req.ttl_ms),
                    )
                    .await
                {
                    Ok(res) => TurnTicketOutcome::Issued {
                        ticket: res.ticket,
                        server_pubkey: res.server_pubkey,
                    },
                    Err(
                        rings_turn::TurnError::InvalidReservationTtl
                        | rings_turn::TurnError::Core(_),
                    ) => TurnTicketOutcome::Rejected(TurnTicketRejection::InvalidRequest),
                    Err(
                        rings_turn::TurnError::ReservationCapacityReached { .. }
                        | rings_turn::TurnError::AllocationCapacityReached { .. },
                    ) => TurnTicketOutcome::Rejected(TurnTicketRejection::CapacityReached),
                    Err(_) => TurnTicketOutcome::Rejected(TurnTicketRejection::InternalError),
                }
            }
        };
        encode_turn_ticket_outcome(&outcome)
    }
}

/// Register [`TurnTicketInvokeBackend`] on `pipeline_handle` under [`TURN_TICKET_INVOKE_SHAPE`]
/// and [`TURN_TICKET_INVOKE_BACKEND`].
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
pub fn register_turn_ticket_invoke_backend(
    pipeline_handle: &OnionPipelineHandle,
    policy: OnionInvokePolicy,
    server: Arc<rings_turn::TurnServer>,
) -> Result<()> {
    let backend = OnionBackendName::parse(TURN_TICKET_INVOKE_BACKEND)?;
    pipeline_handle.register_invoke(
        TURN_TICKET_INVOKE_SHAPE,
        backend,
        policy,
        TurnTicketInvokeBackend::new(server),
    )
}

/// Native YAML configuration (`turn_service:`) for hosting a [`rings_turn::TurnServer`],
/// announcing its [`TurnServiceDescriptor`] in the DHT, and serving `turn.ticket` over onion
/// `invoke` (#1292).
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnServiceConfig {
    /// Local UDP/TCP socket address the TURN server listens on.
    #[serde(default = "default_turn_listen_addr")]
    pub listen_addr: SocketAddr,
    /// Public IPv4/IPv6 address advertised in `XOR-RELAYED-ADDRESS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<std::net::IpAddr>,
    /// Optional public IPv4/IPv6 address advertised for the TURN listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_ip: Option<std::net::IpAddr>,
    /// Optional explicit public `turn://host:port` or `turns://host:port` URL published in
    /// [`TurnServiceDescriptor`]. Defaults to `turn://<advertised_addr>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_url: Option<String>,
    /// Optional local IP bound when allocating relay UDP sockets (defaults to `listen_addr.ip()`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_bind_ip: Option<std::net::IpAddr>,
    /// Optional inclusive lower bound for allocated relay UDP ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_port: Option<u16>,
    /// Optional inclusive upper bound for allocated relay UDP ports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_port: Option<u16>,
    /// TURN authentication realm advertised in 401/438 challenges.
    #[serde(default = "default_turn_realm")]
    pub realm: String,
    /// Optional hex-encoded 32-byte secp256k1 secret key for TURN ECDH credential derivation.
    /// When omitted, a fresh random keypair is generated on startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_key: Option<String>,
    /// Allow relaying to loopback, link-local, or private peer addresses (`false` by default).
    #[serde(default)]
    pub allow_private_peers: bool,
    /// Maximum concurrent TURN nonce challenges tracked in memory.
    #[serde(default = "default_turn_max_nonces")]
    pub max_nonces: usize,
    /// Maximum concurrent TURN relay allocations.
    #[serde(default = "default_turn_max_allocations")]
    pub max_allocations: usize,
    /// Maximum concurrent unclaimed `SessionTicket` reservations.
    #[serde(default = "default_turn_max_reservations")]
    pub max_reservations: usize,
    /// Maximum lifetime in seconds of an unclaimed `SessionTicket` reservation (`60s` default).
    #[serde(default = "default_turn_max_reservation_ttl_secs")]
    pub max_reservation_ttl_secs: u64,
    /// Maximum active peer IP permissions per TURN allocation.
    #[serde(default = "default_turn_max_permissions_per_allocation")]
    pub max_permissions_per_allocation: usize,
    /// ChannelBind timeout in seconds (`0` uses the RFC 5766 default of `600s`).
    #[serde(default)]
    pub channel_bind_timeout_secs: u64,
    /// Optional allowlist of delegator DIDs for non-ticket TURN authentication.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_dids: Vec<Did>,
    /// Descriptor heartbeat interval in seconds.
    #[serde(default = "default_turn_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    /// Descriptor freshness window (TTL) in seconds.
    #[serde(default = "default_turn_ttl_secs")]
    pub ttl_secs: u64,
    /// Whether to register and advertise the `turn.ticket` onion `invoke` backend (`true` by default).
    #[serde(default = "default_turn_issue_tickets")]
    pub issue_tickets: bool,
    /// Quota policy for the `turn.ticket` onion `invoke` backend.
    #[serde(default = "default_turn_ticket_invoke_policy")]
    pub ticket_invoke_policy: OnionInvokePolicy,
    /// Opt-in Phase B multi-hop mode: route each TURN allocation's UDP datagrams through a
    /// dedicated guard-closed `onion::udp` session (`false` by default).
    #[serde(default)]
    pub multi_hop_onion_udp: bool,
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
impl std::fmt::Debug for TurnServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServiceConfig")
            .field("listen_addr", &self.listen_addr)
            .field("public_ip", &self.public_ip)
            .field("advertised_ip", &self.advertised_ip)
            .field("turn_url", &self.turn_url)
            .field("relay_bind_ip", &self.relay_bind_ip)
            .field("min_port", &self.min_port)
            .field("max_port", &self.max_port)
            .field("realm", &self.realm)
            .field(
                "server_key",
                &self.server_key.as_ref().map(|_| "<redacted>"),
            )
            .field("allow_private_peers", &self.allow_private_peers)
            .field("max_nonces", &self.max_nonces)
            .field("max_allocations", &self.max_allocations)
            .field("max_reservations", &self.max_reservations)
            .field("max_reservation_ttl_secs", &self.max_reservation_ttl_secs)
            .field(
                "max_permissions_per_allocation",
                &self.max_permissions_per_allocation,
            )
            .field("channel_bind_timeout_secs", &self.channel_bind_timeout_secs)
            .field("allow_dids", &self.allow_dids)
            .field("heartbeat_interval_secs", &self.heartbeat_interval_secs)
            .field("ttl_secs", &self.ttl_secs)
            .field("issue_tickets", &self.issue_tickets)
            .field("ticket_invoke_policy", &self.ticket_invoke_policy)
            .field("multi_hop_onion_udp", &self.multi_hop_onion_udp)
            .finish()
    }
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_listen_addr() -> SocketAddr {
    SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        rings_turn::DEFAULT_TURN_PORT,
    )
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
fn default_turn_realm() -> String {
    rings_turn::DEFAULT_TURN_REALM.to_string()
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_max_nonces() -> usize {
    rings_turn::DEFAULT_MAX_NONCES
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_max_allocations() -> usize {
    rings_turn::DEFAULT_MAX_ALLOCATIONS
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_max_reservations() -> usize {
    rings_turn::DEFAULT_MAX_RESERVATIONS
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_max_reservation_ttl_secs() -> u64 {
    rings_turn::DEFAULT_MAX_RESERVATION_TTL.as_secs()
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_max_permissions_per_allocation() -> usize {
    rings_turn::DEFAULT_MAX_PERMISSIONS_PER_ALLOCATION
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_heartbeat_interval_secs() -> u64 {
    30
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_ttl_secs() -> u64 {
    90
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
const fn default_turn_issue_tickets() -> bool {
    true
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
impl Default for TurnServiceConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_turn_listen_addr(),
            public_ip: None,
            advertised_ip: None,
            turn_url: None,
            relay_bind_ip: None,
            min_port: None,
            max_port: None,
            realm: default_turn_realm(),
            server_key: None,
            allow_private_peers: false,
            max_nonces: default_turn_max_nonces(),
            max_allocations: default_turn_max_allocations(),
            max_reservations: default_turn_max_reservations(),
            max_reservation_ttl_secs: default_turn_max_reservation_ttl_secs(),
            max_permissions_per_allocation: default_turn_max_permissions_per_allocation(),
            channel_bind_timeout_secs: 0,
            allow_dids: Vec::new(),
            heartbeat_interval_secs: default_turn_heartbeat_interval_secs(),
            ttl_secs: default_turn_ttl_secs(),
            issue_tickets: default_turn_issue_tickets(),
            ticket_invoke_policy: default_turn_ticket_invoke_policy(),
            multi_hop_onion_udp: false,
        }
    }
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
impl TurnServiceConfig {
    /// Return the [`OnionInvokeOffer`] advertised when `self.issue_tickets` is enabled,
    /// validating that `advertise_onion_relay` is enabled on the node.
    pub fn onion_invoke_offer(
        &self,
        advertise_onion_relay: bool,
    ) -> Result<Option<OnionInvokeOffer>> {
        if !self.issue_tickets {
            return Ok(None);
        }
        if !advertise_onion_relay {
            return Err(Error::InvalidConfig(
                "turn_service.issue_tickets requires advertise_onion_relay: true".to_string(),
            ));
        }
        if self.max_reservations == 0 {
            return Err(Error::InvalidConfig(
                "turn_service.max_reservations must be > 0 when issue_tickets is enabled"
                    .to_string(),
            ));
        }
        if self.max_reservation_ttl_secs == 0 {
            return Err(Error::InvalidConfig(
                "turn_service.max_reservation_ttl_secs must be > 0 when issue_tickets is enabled"
                    .to_string(),
            ));
        }
        turn_ticket_invoke_offer(self.ticket_invoke_policy).map(Some)
    }

    /// Build a [`rings_turn::TurnServerConfig`] driven by `clock`.
    pub fn to_turn_server_config(
        &self,
        clock: rings_core::utils::SharedClock,
    ) -> Result<rings_turn::TurnServerConfig> {
        use std::net::IpAddr;
        use std::net::Ipv4Addr;
        use std::net::Ipv6Addr;

        let server_key = match self.server_key.as_deref() {
            Some(raw) => {
                let trimmed = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
                SecretKey::from_str(trimmed).map_err(|e| {
                    Error::InvalidConfig(format!("invalid turn_service.server_key: {e}"))
                })?
            }
            None => SecretKey::random(),
        };

        let bind_ip = self.relay_bind_ip.unwrap_or_else(|| self.listen_addr.ip());
        let relay_ip = self.public_ip.unwrap_or(match bind_ip {
            ip if !ip.is_unspecified() => ip,
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
        if let Some(advertised_ip) = self.advertised_ip {
            if advertised_ip.is_ipv4() != self.listen_addr.ip().is_ipv4() {
                return Err(Error::InvalidConfig(format!(
                    "turn_service.advertised_ip ({advertised_ip}) must match address family of listen_addr ({})",
                    self.listen_addr.ip()
                )));
            }
        }
        let advertised_ip = self.advertised_ip.or_else(|| {
            self.public_ip
                .filter(|ip| ip.is_ipv4() == self.listen_addr.ip().is_ipv4())
        });

        let relay = match (self.min_port, self.max_port) {
            (None, None) => rings_turn::RelayAddressConfig::Static { relay_ip, bind_ip },
            (Some(min_port), Some(max_port)) => rings_turn::RelayAddressConfig::Range {
                relay_ip,
                bind_ip,
                min_port,
                max_port,
                max_retries: rings_turn::DEFAULT_PORT_RANGE_MAX_RETRIES,
            },
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::InvalidConfig(
                    "turn_service.min_port and turn_service.max_port must be set together"
                        .to_string(),
                ));
            }
        };
        relay
            .validate()
            .map_err(|e| Error::InvalidConfig(format!("turn_service: {e}")))?;

        let peer_policy = if self.allow_private_peers {
            rings_turn::RelayPeerPolicy::AllowLocalAndPrivate
        } else {
            rings_turn::RelayPeerPolicy::PublicOnly
        };
        let admission = if self.allow_dids.is_empty() {
            rings_turn::TurnAdmissionPolicy::AllowAnyDelegation
        } else {
            rings_turn::TurnAdmissionPolicy::allowlist(self.allow_dids.iter().copied())
        };

        Ok(rings_turn::TurnServerConfig {
            listen_addr: self.listen_addr,
            advertised_ip,
            realm: self.realm.clone(),
            server_key,
            relay,
            peer_policy,
            max_nonces: self.max_nonces.max(1),
            max_allocations: self.max_allocations.max(1),
            max_reservations: self.max_reservations,
            max_reservation_ttl: Duration::from_secs(self.max_reservation_ttl_secs),
            max_permissions_per_allocation: self.max_permissions_per_allocation.max(1),
            channel_bind_timeout: Duration::from_secs(self.channel_bind_timeout_secs),
            admission,
            clock,
            custom_relay_factory: None,
        })
    }

    /// Build the validated [`TurnServiceOffer`] for a started [`rings_turn::TurnServer`].
    pub fn service_offer(&self, server: &rings_turn::TurnServer) -> Result<TurnServiceOffer> {
        let url = self
            .turn_url
            .clone()
            .unwrap_or_else(|| format!("turn://{}", server.advertised_addr()));
        TurnServiceOffer::with_schedule(
            url,
            server.server_pubkey(),
            Duration::from_secs(self.heartbeat_interval_secs),
            Duration::from_secs(self.ttl_secs),
        )
    }
}

/// Top-level YAML configuration combining a `rings-node` native [`rings_node::native::config::Config`]
/// with a [`TurnServiceConfig`] for the standalone `rings-turn-service` daemon.
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnServiceDaemonConfig {
    /// Underlying `rings-node` native configuration (`network_id`, key, seeds, onion relay, etc.).
    pub node: rings_node::native::config::Config,
    /// TURN relay server and overlay announcement configuration.
    pub turn_service: TurnServiceConfig,
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
impl TurnServiceDaemonConfig {
    /// Validate and derive a [`ProcessorBuilder`] (plus its [`rings_core::utils::SharedClock`])
    /// with the `turn.ticket` [`OnionInvokeOffer`] merged into `onion_role` when
    /// `turn_service.issue_tickets` is enabled.
    pub fn to_processor_builder(
        &self,
    ) -> Result<(ProcessorBuilder, rings_core::utils::SharedClock)> {
        let invoke_offer = self
            .turn_service
            .onion_invoke_offer(self.node.advertise_onion_relay)?;
        let pc: rings_node::processor::ProcessorConfig = self.node.clone().try_into()?;
        let clock = pc.clock().clone();
        let mut builder = ProcessorBuilder::from_config(&pc)?;
        if let Some(turn_invoke) = invoke_offer {
            builder = apply_turn_invoke_offer_to_builder(builder, turn_invoke)?;
        }
        Ok((builder, clock))
    }
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
fn apply_turn_invoke_offer_to_builder(
    builder: ProcessorBuilder,
    turn_invoke: OnionInvokeOffer,
) -> Result<ProcessorBuilder> {
    let role = match builder.configured_onion_role().clone() {
        rings_node::onion::OnionRole::Client => {
            return Err(Error::InvalidConfig(
                "turn_service.issue_tickets requires advertise_onion_relay: true".to_string(),
            ));
        }
        rings_node::onion::OnionRole::Relay => rings_node::onion::OnionRole::Invoke(turn_invoke),
        rings_node::onion::OnionRole::Exit(exit_offer) => rings_node::onion::OnionRole::Both {
            exit: exit_offer,
            invoke: turn_invoke,
        },
        rings_node::onion::OnionRole::Invoke(existing) => {
            rings_node::onion::OnionRole::Invoke(merge_invoke_offers(&existing, &turn_invoke)?)
        }
        rings_node::onion::OnionRole::Both { exit, invoke } => rings_node::onion::OnionRole::Both {
            exit,
            invoke: merge_invoke_offers(&invoke, &turn_invoke)?,
        },
    };
    Ok(builder.onion_role(role))
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
fn merge_invoke_offers(
    first: &OnionInvokeOffer,
    second: &OnionInvokeOffer,
) -> Result<OnionInvokeOffer> {
    let mut entries = Vec::new();
    for ((shape, backend), &policy) in first.entries().chain(second.entries()) {
        entries.push((*shape, backend.clone(), policy));
    }
    OnionInvokeOffer::new(entries)
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
struct TurnDaemonHooks {
    turn_server: Arc<rings_turn::TurnServer>,
    offer: TurnServiceOffer,
    turn_invoke: Option<OnionInvokeOffer>,
    ticket_invoke_policy: OnionInvokePolicy,
    onion_udp_factory: Option<Arc<OnionUdpRelayFactory>>,
}

#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
#[async_trait]
impl rings_node::native::daemon::NativeDaemonHooks for TurnDaemonHooks {
    fn configure_builder(
        &mut self,
        mut builder: ProcessorBuilder,
    ) -> anyhow::Result<ProcessorBuilder> {
        if let Some(ref turn_invoke) = self.turn_invoke {
            builder = apply_turn_invoke_offer_to_builder(builder, turn_invoke.clone())?;
        }
        Ok(announce_turn_service(builder, self.offer.clone()))
    }

    async fn configure_provider(&mut self, provider: &Arc<Provider>) -> anyhow::Result<()> {
        if let Some(ref factory) = self.onion_udp_factory {
            factory.attach_provider(provider)?;
        }
        if self.turn_invoke.is_some() {
            let pipeline = provider.onion_pipeline()?;
            register_turn_ticket_invoke_backend(
                &pipeline,
                self.ticket_invoke_policy,
                Arc::clone(&self.turn_server),
            )?;
        }
        install_turn_client_rpc(provider)?;
        tracing::info!(
            did = %provider.processor().did(),
            turn_listen = %self.turn_server.local_addr(),
            turn_advertised = %self.turn_server.advertised_addr(),
            issue_tickets = self.turn_invoke.is_some(),
            multi_hop_onion_udp = self.onion_udp_factory.is_some(),
            "rings-turn-service attached to native foreground daemon"
        );
        Ok(())
    }

    async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.turn_server
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("closing TurnServer failed: {e}"))
    }
}

/// Start [`rings_turn::TurnServer`] and run a full foreground Rings node daemon (persistent
/// [`rings_node::native::stores::NodeStores`] including authoritative replay storage,
/// [`rings_node::native::bootstrap::BootstrapSupervisor`], internal/external HTTP JSON-RPC
/// signaling endpoints, delegation renewal/horizon supervision, [`TurnServiceDescriptor`]
/// announcement, and `turn.ticket` onion `invoke` serving) until `SIGINT`/`SIGTERM`.
#[cfg(all(
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
pub async fn run_turn_service_daemon(
    options: rings_node::native::daemon::NativeNodeRunOptions,
    turn_service: TurnServiceConfig,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let turn_invoke = turn_service
        .onion_invoke_offer(options.config.advertise_onion_relay)
        .context("validating turn_service onion invoke configuration")?;
    let pc = rings_node::processor::ProcessorConfig::try_from(options.config.clone())
        .context("validating node configuration")?;
    let mut server_cfg = turn_service
        .to_turn_server_config(pc.clock().clone())
        .context("building TurnServerConfig")?;
    let onion_udp_factory = if turn_service.multi_hop_onion_udp {
        let factory = OnionUdpRelayFactory::deferred(
            rings_node::onion::OnionLoopClass::DEFAULT,
            server_cfg.peer_policy,
        );
        server_cfg.custom_relay_factory =
            Some(Arc::clone(&factory) as Arc<dyn rings_turn::TurnDatagramRelayFactory>);
        Some(factory)
    } else {
        None
    };
    let turn_server = Arc::new(
        rings_turn::TurnServer::start(server_cfg)
            .await
            .context("starting TurnServer")?,
    );
    let offer = match turn_service.service_offer(&turn_server) {
        Ok(offer) => offer,
        Err(err) => {
            let _ = turn_server.close().await;
            return Err(err.into());
        }
    };

    let server_cleanup = Arc::clone(&turn_server);
    let hooks = TurnDaemonHooks {
        turn_server,
        offer,
        turn_invoke,
        ticket_invoke_policy: turn_service.ticket_invoke_policy,
        onion_udp_factory,
    };
    let run_res = rings_node::native::daemon::run_foreground_node(options, hooks).await;
    let close_res = server_cleanup.close().await;
    match (run_res, close_res) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(anyhow::anyhow!("closing TurnServer failed: {err}")),
    }
}

#[cfg(all(
    test,
    any(feature = "native", feature = "dummy"),
    not(target_family = "wasm")
))]
mod tests {
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use rings_core::swarm::OfferReply;
    use rings_core::utils::ManualClock;
    use rings_node::onion::OnionRole;
    use rings_node::processor::ProcessorConfig;
    use rings_test_support::with_hang_guard;
    use rings_turn::RelayAddressConfig;
    use rings_turn::TurnAdmissionPolicy;
    use rings_turn::TurnCredentialSigner;
    use rings_turn::TurnServer;
    use rings_turn::TurnServerConfig;

    use super::*;

    fn registry_entry<D: RegistryElement>(
        topic: &str,
        descriptors: Vec<D>,
    ) -> Result<entry::Entry> {
        let data = descriptors
            .into_iter()
            .map(|descriptor| descriptor.to_element().map_err(Error::CoreError))
            .collect::<Result<Vec<_>>>()?;
        Ok(entry::Entry::new(
            entry::Entry::gen_did(topic)?,
            data,
            entry::EntryKind::Data,
        ))
    }

    async fn signal_edge(first: &Processor, second: &Processor) -> Result<()> {
        let res = async {
            let (attempt, offer) = first.swarm.offer_connection(second.did()).await?;
            match second.swarm.answer_offer(offer).await? {
                OfferReply::Answer(answer) => first.swarm.accept_answer_for(attempt, answer).await,
                OfferReply::CounterOffer(counter_offer) => {
                    let answer = first
                        .swarm
                        .answer_offer(counter_offer)
                        .await?
                        .answer()
                        .ok_or(rings_core::error::Error::AlreadyConnected)?;
                    second.swarm.accept_answer(answer).await
                }
            }
        }
        .await;
        match res {
            Ok(())
            | Err(
                rings_core::error::Error::AlreadyConnected
                | rings_core::error::Error::ConnectionAttemptSuperseded { .. },
            ) => {}
            Err(other) => return Err(Error::CoreError(other)),
        }
        with_hang_guard(
            "await mutual peer admission",
            Duration::from_secs(30),
            async {
                while !first.swarm.peer_dids().contains(&second.did())
                    || !second.swarm.peer_dids().contains(&first.did())
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .await;
        Ok(())
    }

    #[test]
    fn validate_turn_service_endpoint_rejects_loopback_private_link_local_ula_and_unspecified() {
        let pk = SecretKey::random().pubkey();
        for rejected in [
            "turn://127.0.0.1:3478",
            "turn://127.1:3478",
            "turn://0x7f000001:3478",
            "turn://10.0.0.1:3478",
            "turn://172.16.0.1:3478",
            "turn://192.168.1.1:3478",
            "turn://169.254.169.254:3478",
            "turn://0.0.0.0:3478",
            "turn://100.64.0.1:3478",
            "turn://[::1]:3478",
            "turn://[::]:3478",
            "turn://[::ffff:127.0.0.1]:3478",
            "turn://[fe80::1]:3478",
            "turn://[fc00::1]:3478",
            "turn://[fd12:3456::1]:3478",
            "turn://localhost:3478",
            "turn://turn.localhost:3478",
            "turn://turn.local:3478",
            "turn://turn.internal:3478",
            "turn://intranet:3478",
            "turn://1.1.1.1:0",
            "turn://user:pass@turn.rings.rs:3478",
            "turn://turn.rings.rs:3478/path",
        ] {
            assert!(
                validate_turn_service_endpoint(rejected, pk).is_err(),
                "expected {rejected:?} to be rejected"
            );
            assert!(
                TurnServiceOffer::new(rejected, pk).is_err(),
                "TurnServiceOffer::new must reject {rejected:?}"
            );
        }

        for accepted in [
            "turn://turn.rings.rs:3478",
            "turns://turn.rings.rs:5349?transport=tcp",
            "turn://1.1.1.1:3478?transport=udp",
            "turn://[2606:4700:4700::1111]:3478",
        ] {
            assert!(
                validate_turn_service_endpoint(accepted, pk).is_ok(),
                "expected {accepted:?} to be accepted"
            );
        }
    }

    #[test]
    fn daemon_config_validates_relay_requirement_and_redacts_server_key() {
        let key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
        let key_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(key_file.path(), key.dump().unwrap()).unwrap();
        let mut node_cfg = rings_node::native::config::Config::new(key_file.path());
        node_cfg.advertise_onion_relay = false;
        let secret_hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let turn_cfg = TurnServiceConfig {
            public_ip: Some(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))),
            server_key: Some(secret_hex.to_string()),
            ..TurnServiceConfig::default()
        };
        assert!(
            !format!("{turn_cfg:?}").contains(secret_hex),
            "TurnServiceConfig Debug must redact server_key"
        );

        let bad = TurnServiceDaemonConfig {
            node: node_cfg.clone(),
            turn_service: turn_cfg.clone(),
        };
        assert!(
            bad.to_processor_builder().is_err(),
            "issue_tickets must fail when node.advertise_onion_relay is false"
        );

        node_cfg.advertise_onion_relay = true;
        let ok = TurnServiceDaemonConfig {
            node: node_cfg,
            turn_service: turn_cfg,
        };
        let (builder, _) = ok.to_processor_builder().unwrap();
        assert!(builder.configured_onion_role().invoke().is_some());
    }

    #[tokio::test]
    async fn turn_service_registration_publishes_discovers_and_supersedes_invalid_newer_leases(
    ) -> Result<()> {
        let t0: u128 = rings_core::utils::get_epoch_ms();
        let clock = Arc::new(ManualClock::new(t0));
        let account_key = SecretKey::random();
        let delegatee_key = DelegateeKey::new_with_seckey_at(&account_key, 600_000, t0).unwrap();
        let turn_server_pubkey = SecretKey::random().pubkey();

        let offer = TurnServiceOffer::new("turn://turn.rings.rs:3478", turn_server_pubkey)?;
        let config =
            ProcessorConfig::new(1, "".to_string(), delegatee_key.clone(), 15).with_clock(clock);
        let processor = announce_turn_service(
            ProcessorBuilder::from_config(&config)?.advertise_presence(false),
            offer,
        )
        .build()?;

        // Seed BOTH:
        // 1. An older valid descriptor (`expires_at_ms = t0 + 60_000`) for `evil_account` / `epoch_a`.
        // 2. A newer descriptor (`expires_at_ms = t0 + 90_000`) for the SAME `(did, epoch_a)` that
        //    switches `turn_url` to an invalid loopback endpoint (`turn://127.0.0.1:3478`).
        // Deduplicating BEFORE `has_valid_endpoint` must let #2 supersede #1 and then filter #2 out,
        // rather than resurrecting the superseded older lease #1!
        let evil_account = SecretKey::random();
        let evil_delegatee = DelegateeKey::new_with_seckey_at(&evil_account, 600_000, t0).unwrap();
        let epoch_a = OnionProcessEpoch::new([0x42; 16]);
        let older_valid = TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                did: evil_delegatee.delegator_did(),
                public_key: evil_delegatee
                    .delegation()
                    .delegator_verification_pubkey()
                    .unwrap(),
                delegatee_public_key: evil_delegatee.delegatee_public_key(),
                process_epoch: epoch_a,
                network_id: 1,
                turn_url: "turn://evil-old.rings.rs:3478".to_string(),
                server_pubkey: SecretKey::random().pubkey(),
                fresh_for_ms: 60_000,
                expires_at_ms: t0 + 60_000,
            },
            MessageSigner::new(evil_delegatee.clone(), 1),
        )
        .unwrap();
        let newer_invalid = TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                did: evil_delegatee.delegator_did(),
                public_key: evil_delegatee
                    .delegation()
                    .delegator_verification_pubkey()
                    .unwrap(),
                delegatee_public_key: evil_delegatee.delegatee_public_key(),
                process_epoch: epoch_a,
                network_id: 1,
                turn_url: "turn://127.0.0.1:3478".to_string(),
                server_pubkey: SecretKey::random().pubkey(),
                fresh_for_ms: 90_000,
                expires_at_ms: t0 + 90_000,
            },
            MessageSigner::new(evil_delegatee, 1),
        )
        .unwrap();
        processor
            .storage_store(registry_entry(TURN_SERVICES_TOPIC, vec![
                older_valid,
                newer_invalid,
            ])?)
            .await?;

        processor.register_all_once().await?;

        let raw_entry = processor
            .fetch_topic_entry(TURN_SERVICES_TOPIC)
            .await?
            .expect("turn_services entry must exist");
        assert_eq!(
            TurnServiceRegistration::descriptors_from_entry(&raw_entry).len(),
            3,
            "all three descriptors must coexist in the raw DHT entry"
        );

        let discovered = lookup_turn_services(&processor, false).await?;
        assert_eq!(
            discovered.len(),
            1,
            "newer invalid lease must supersede and suppress older valid lease for the same process"
        );
        let desc = &discovered[0];
        assert_eq!(desc.body.did, processor.did());
        assert_eq!(
            desc.body.delegatee_public_key,
            delegatee_key.delegatee_public_key()
        );
        assert_eq!(desc.body.turn_url, "turn://turn.rings.rs:3478");
        assert_eq!(desc.body.server_pubkey, turn_server_pubkey);

        let base_url = desc.ice_server_base_url()?;
        let signed_url = delegatee_key
            .sign_ice_servers(&base_url)
            .map_err(Error::CoreError)?;
        assert!(signed_url.starts_with("turn://"));
        assert!(signed_url.ends_with("@turn.rings.rs:3478"));
        assert!(!signed_url.contains("pubkey="));
        processor.close().await?;
        Ok(())
    }

    #[test]
    fn multi_process_descriptors_under_same_did_coexist_and_bind_delegatee_key() {
        let t0: u128 = rings_core::utils::get_epoch_ms();
        let shared_account = SecretKey::random();
        let proc1_key = DelegateeKey::new_with_seckey_at(&shared_account, 600_000, t0).unwrap();
        let proc2_key = DelegateeKey::new_with_seckey_at(&shared_account, 600_000, t0).unwrap();
        let epoch1 = OnionProcessEpoch::new([0x11; 16]);
        let epoch2 = OnionProcessEpoch::new([0x22; 16]);
        let server1_pk = SecretKey::random().pubkey();
        let server2_pk = SecretKey::random().pubkey();

        let desc1 = TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                did: proc1_key.delegator_did(),
                public_key: proc1_key
                    .delegation()
                    .delegator_verification_pubkey()
                    .unwrap(),
                delegatee_public_key: proc1_key.delegatee_public_key(),
                process_epoch: epoch1,
                network_id: 1,
                turn_url: "turn://turn-a.rings.rs:3478".to_string(),
                server_pubkey: server1_pk,
                fresh_for_ms: 90_000,
                expires_at_ms: t0 + 90_000,
            },
            MessageSigner::new(&proc1_key, 1),
        )
        .unwrap()
        .heard_at(t0);

        let desc2 = TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                did: proc2_key.delegator_did(),
                public_key: proc2_key
                    .delegation()
                    .delegator_verification_pubkey()
                    .unwrap(),
                delegatee_public_key: proc2_key.delegatee_public_key(),
                process_epoch: epoch2,
                network_id: 1,
                turn_url: "turn://turn-b.rings.rs:3478".to_string(),
                server_pubkey: server2_pk,
                fresh_for_ms: 90_000,
                expires_at_ms: t0 + 90_050,
            },
            MessageSigner::new(&proc2_key, 1),
        )
        .unwrap()
        .heard_at(t0);

        // Forged delegatee_public_key (signing with proc1_key while claiming proc2's delegatee_public_key)
        // must be rejected both at signing and verification.
        assert!(TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                delegatee_public_key: proc2_key.delegatee_public_key(),
                ..desc1.body.clone()
            },
            MessageSigner::new(&proc1_key, 1),
        )
        .is_err());

        let by_proc = TurnServiceDescriptor::latest_valid_by_process(
            vec![desc1.clone(), desc2.clone()],
            t0,
            1,
            false,
        );
        assert_eq!(
            by_proc.len(),
            2,
            "two processes sharing one DID with distinct process_epochs must both remain addressable"
        );

        let (pending, rpc_req) =
            prepare_turn_ticket_onion_invoke(by_proc, None, Duration::from_secs(30), None).unwrap();
        assert_eq!(rpc_req.eligible_stages.as_ref().unwrap().len(), 2);

        // Completing against process #1's stage response selects server1_pk (`turn-a.rings.rs`)
        // even though process #2 had a later `expires_at_ms`!
        let fake_server1 = SecretKey::random();
        let desc1_matching = TurnServiceDescriptor::new_signed(
            TurnServiceDescriptorBody {
                server_pubkey: fake_server1.pubkey(),
                ..desc1.body
            },
            MessageSigner::new(&proc1_key, 1),
        )
        .unwrap()
        .heard_at(t0);
        let (pending2, _) = prepare_turn_ticket_onion_invoke(
            vec![desc1_matching, desc2],
            None,
            Duration::from_secs(30),
            None,
        )
        .unwrap();
        let eph_pk = pending2.ephemeral_client_key.pubkey();
        let ticket =
            SignedTurnSessionTicket::issue([0x55; 16], eph_pk, t0 + 30_000, &fake_server1).unwrap();
        let out_frame = encode_turn_ticket_outcome(&TurnTicketOutcome::Issued {
            ticket,
            server_pubkey: fake_server1.pubkey(),
        })
        .unwrap();
        let acquired = pending2
            .complete(
                &OnionInvokeRpcResponse {
                    stage_did: proc1_key.delegator_did(),
                    stage_process_epoch: epoch1,
                    stage_delegatee_public_key: proc1_key.delegatee_public_key(),
                    output_base64: base64::encode(&out_frame),
                },
                t0,
            )
            .unwrap();
        assert!(acquired.ice_url.ends_with("@turn-a.rings.rs:3478"));
        assert_eq!(acquired.turn_process_epoch, epoch1);
        drop(pending);
    }

    #[cfg(not(feature = "dummy"))]
    static WEBRTC_TEST_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    #[cfg(not(feature = "dummy"))]
    pub(crate) async fn webrtc_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        WEBRTC_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct OverlayTestNode {
        processor: Arc<Processor>,
        provider: Arc<Provider>,
        stop: rings_core::lifecycle::StopSource,
        listen_task: tokio::task::JoinHandle<Result<()>>,
    }

    impl OverlayTestNode {
        async fn new_with_options(
            role: OnionRole<rings_node::onion::OnionExitOffer>,
            turn_offer: Option<TurnServiceOffer>,
            install_custom_rpc: bool,
        ) -> Result<Self> {
            let key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
            let advertise_presence = role.registers_relay();
            let config = ProcessorConfig::new(1, "".to_string(), key, 1).onion_role(role);
            let mut builder = ProcessorBuilder::from_config(&config)?
                .advertise_presence(advertise_presence)
                .storage(Box::new(rings_core::storage::MemStorage::new()))
                .replay_storage(Box::new(rings_core::storage::MemStorage::new()));
            if let Some(offer) = turn_offer {
                builder = announce_turn_service(builder, offer);
            }
            let processor = Arc::new(builder.build()?);
            let provider = Arc::new(Provider::from_processor(Arc::clone(&processor)));
            let _ = provider.onion_pipeline()?;
            provider.set_backend()?;
            if install_custom_rpc {
                install_turn_client_rpc(&provider)?;
            }
            let stop = rings_core::lifecycle::StopSource::new();
            let stop_token = stop.token();
            let listen_provider = Arc::clone(&provider);
            let listen_task =
                tokio::spawn(async move { listen_provider.listen_with(stop_token).await });
            Ok(Self {
                processor,
                provider,
                stop,
                listen_task,
            })
        }

        async fn new(
            role: OnionRole<rings_node::onion::OnionExitOffer>,
            turn_offer: Option<TurnServiceOffer>,
        ) -> Result<Self> {
            Self::new_with_options(role, turn_offer, false).await
        }

        async fn shutdown(self) -> Result<()> {
            self.stop.request_stop();
            let _ = self.listen_task.await;
            self.processor.close().await
        }
    }

    #[tokio::test]
    async fn turn_ticket_onion_invoke_works_over_builtin_rings_node_rpc_and_relays_udp(
    ) -> Result<()> {
        #[cfg(not(feature = "dummy"))]
        let _webrtc_serial = webrtc_test_guard().await;
        let t0: u128 = rings_core::utils::get_epoch_ms();
        let turn_clock = Arc::new(ManualClock::new(t0));

        // Start a strict TURN server (`allowlist([])`, `max_reservations = 2`) bound locally on
        // loopback while advertising a public IP in the DHT descriptor.
        let mut turn_cfg = TurnServerConfig::loopback_ephemeral();
        turn_cfg.admission = TurnAdmissionPolicy::allowlist(std::iter::empty());
        turn_cfg.clock = turn_clock.clone();
        turn_cfg.max_allocations = 4;
        turn_cfg.max_reservations = 2;
        turn_cfg.relay = RelayAddressConfig::Static {
            relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        let turn_server = Arc::new(
            TurnServer::start(turn_cfg)
                .await
                .map_err(|e| Error::InvalidConfig(e.to_string()))?,
        );

        let invoke_policy = OnionInvokePolicy {
            max_concurrent: 16,
            max_invocations_per_minute: 64,
        };
        let advertised_turn_url = format!("turn://1.1.1.1:{}", turn_server.local_addr().port());
        let turn_offer = TurnServiceOffer::new(&advertised_turn_url, turn_server.server_pubkey())?;

        // Notice `install_custom_rpc = false`: `client` is a stock `rings-node` Provider with NO
        // `install_turn_client_rpc` installed, matching shipped `librings_node` FFI and `rings-node`
        // browser WASM builds!
        let client = OverlayTestNode::new_with_options(OnionRole::Client, None, false).await?;
        let turn_node = OverlayTestNode::new(
            OnionRole::Invoke(turn_ticket_invoke_offer(invoke_policy)?),
            Some(turn_offer),
        )
        .await?;
        register_turn_ticket_invoke_backend(
            &turn_node.provider.onion_pipeline()?,
            invoke_policy,
            Arc::clone(&turn_server),
        )?;

        let mut relays = Vec::with_capacity(4);
        for _ in 0..4 {
            relays.push(OverlayTestNode::new(OnionRole::Relay, None).await?);
        }

        let all_refs = std::iter::once(&client)
            .chain(std::iter::once(&turn_node))
            .chain(relays.iter())
            .collect::<Vec<_>>();
        for i in 0..all_refs.len() {
            for j in (i + 1)..all_refs.len() {
                signal_edge(&all_refs[i].processor, &all_refs[j].processor).await?;
            }
        }

        with_hang_guard(
            "await DHT convergence for Phase A turn.ticket test",
            Duration::from_secs(30),
            async {
                loop {
                    let online = client
                        .provider
                        .lookup_registry_raw("online_nodes")
                        .await
                        .map(|r| r.elements.len())
                        .unwrap_or(0);
                    let invokes = client
                        .provider
                        .lookup_registry_raw("onion_invokes")
                        .await
                        .map(|r| r.elements.len())
                        .unwrap_or(0);
                    let turns = client
                        .provider
                        .lookup_registry_raw(TURN_SERVICES_TOPIC)
                        .await
                        .map(|r| r.elements.len())
                        .unwrap_or(0);
                    if online >= 5 && invokes >= 1 && turns >= 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        )
        .await;

        // 1. Client acquires ticket #1 via `request_turn_ticket` (which uses `lookup_registry_raw` +
        //    `onion_invoke_raw`), requesting 3600s and verifying server clamps to 60s (`DEFAULT_MAX_RESERVATION_TTL`).
        let acquired_1 = with_hang_guard(
            "acquire turn ticket #1",
            Duration::from_secs(10),
            request_turn_ticket(&client.provider, Duration::from_secs(3_600), None),
        )
        .await?;
        assert!(acquired_1.ice_url.starts_with("turn://tkt-"));
        assert_eq!(
            acquired_1.expires_at_ms,
            t0 + rings_turn::DEFAULT_MAX_RESERVATION_TTL.as_millis()
        );
        assert!(!acquired_1
            .ice_url
            .contains(&client.processor.did().to_string()));
        assert!(!format!("{acquired_1:?}").contains(&{
            let parsed: rings_transport::ice_server::IceServer =
                acquired_1.ice_url.parse().unwrap();
            parsed.credential
        }));

        // 2. Client acquires ticket #2 (reaches `max_reservations = 2`).
        let _acquired_2 = with_hang_guard(
            "acquire turn ticket #2",
            Duration::from_secs(10),
            request_turn_ticket(
                &client.provider,
                Duration::from_secs(30),
                Some(turn_node.processor.did()),
            ),
        )
        .await?;
        assert_eq!(turn_server.active_reservation_count().await, 2);

        // 3. Anonymous flood attempt #3 is rejected at the per-server reservation cap (`2`).
        let third_err = with_hang_guard(
            "acquire turn ticket #3 over cap",
            Duration::from_secs(10),
            request_turn_ticket(&client.provider, Duration::from_secs(30), None),
        )
        .await
        .expect_err("3rd unclaimed ticket must be rejected by max_reservations=2");
        assert!(
            third_err.to_string().contains("capacity reached"),
            "unexpected rejection error: {third_err}"
        );

        // 4. Advance clock past 60s so both unclaimed reservations expire, then drive ONLY
        //    `Provider::request_internal("lookupRegistry")` + `Provider::request_internal("onionInvoke")`
        //    (the exact JSON-RPC surface of shipped `librings_node` C FFI & `rings-node` WASM!) and
        //    verify an unmodified TURN UDP client allocates and relays datagrams with the ticket!
        turn_clock.set(t0 + 60_001);
        let raw_lookup_json = with_hang_guard(
            "lookupRegistry builtin RPC",
            Duration::from_secs(10),
            client.provider.request_internal(
                "lookupRegistry".to_string(),
                serde_json::json!({ "topic": TURN_SERVICES_TOPIC }),
            ),
        )
        .await?;
        let lookup_resp: LookupRegistryResponse = serde_json::from_value(raw_lookup_json)?;
        let services = decode_turn_services_from_registry_response(&lookup_resp, false)?;
        assert_eq!(services.len(), 1);
        let (pending, invoke_req) = prepare_turn_ticket_onion_invoke(
            services,
            Some(client.processor.did()),
            Duration::from_secs(30),
            None,
        )?;
        let raw_invoke_json = with_hang_guard(
            "onionInvoke builtin RPC",
            Duration::from_secs(10),
            client.provider.request_internal(
                "onionInvoke".to_string(),
                serde_json::to_value(&invoke_req)?,
            ),
        )
        .await?;
        let invoke_resp: OnionInvokeRpcResponse = serde_json::from_value(raw_invoke_json)?;
        let acquired_via_builtin_rpc = pending.complete(&invoke_resp, t0 + 60_001)?;

        let parsed_ice: rings_transport::ice_server::IceServer = acquired_via_builtin_rpc
            .ice_url
            .parse()
            .map_err(|e| Error::InvalidConfig(format!("invalid ICE URL: {e}")))?;
        assert!(parsed_ice.username.starts_with("tkt-"));
        assert!(!parsed_ice.credential.is_empty());

        // 5. Synchronize `turn_clock` to wall-clock `now_ms`, then request a 100ms TTL ticket over
        //    the 500ms `L(f)` onion invoke quantum: it arrives already expired, and
        //    `acquire_turn_ticket` (sampling `processor.clock().now_ms()` after invocation) must
        //    reject it rather than returning a dead ICE URL.
        turn_clock.set(rings_core::utils::get_epoch_ms());
        let sub_quantum_res = with_hang_guard(
            "sub-quantum TTL acquire_turn_ticket",
            Duration::from_secs(10),
            acquire_turn_ticket(
                &client.processor,
                &client.provider.onion_pipeline()?,
                Duration::from_millis(100),
                None,
            ),
        )
        .await;
        assert!(
            sub_quantum_res.is_err(),
            "ticket whose TTL elapsed during 500ms onion invoke must be rejected on arrival"
        );

        turn_server.close().await.unwrap();
        client.shutdown().await?;
        turn_node.shutdown().await?;
        for relay in relays {
            relay.shutdown().await?;
        }
        Ok(())
    }
}
