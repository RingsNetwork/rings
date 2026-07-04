#![warn(missing_docs)]
//! Application-layer onion routing directory and route selection.
//!
//! This module deliberately sits in `rings-node`, not `rings-core`: Chord
//! remains the storage and discovery substrate, while onion exit policy is an
//! application protocol decision.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use rings_core::dht::Did;
use rings_core::ecc::VerificationPublicKey;
use rings_core::error::Error as CoreError;
use rings_core::error::Result as CoreResult;
use rings_core::measure::order_peers_by_quality;
use rings_core::measure::PeerQuality;
use rings_core::message::Decoder;
use rings_core::message::Encoded;
use rings_core::message::Encoder;
use rings_core::message::MessageVerification;
use rings_core::session::SessionSk;
use rings_core::utils::get_epoch_ms;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeType;
use crate::registration::DhtRegistrationPublisher;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;

/// DHT topic used for application-layer onion exit descriptors.
pub const ONION_EXITS_TOPIC: &str = "onion_exits";

/// Default number of DID hops in a production onion route, including the exit.
pub const DEFAULT_ONION_ROUTE_HOPS: usize = 3;

/// Capability label for nodes willing to relay onion cells.
pub const ONION_RELAY_CAPABILITY: &str = "onion-relay";

/// Capability label for nodes willing to publish onion exit policy.
pub const ONION_EXIT_CAPABILITY: &str = "onion-exit";

/// Application transport exposed by an onion exit service.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum OnionExitTransport {
    /// Native TCP service.
    Tcp,
    /// Native UDP service.
    Udp,
    /// Browser/WebTransport-backed service.
    WebTransport,
    /// Protocol-specific request/response service.
    RequestResponse,
}

/// One named service offered by an onion exit.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitService {
    /// Service name advertised to route builders.
    pub name: String,
    /// Transport used by this service.
    pub transport: OnionExitTransport,
}

impl OnionExitService {
    /// Return whether this service has the requested name.
    pub fn has_name(&self, service: &str) -> bool {
        self.name == service
    }
}

/// Signed policy fields for an onion exit.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitPolicy {
    /// Target allow-list entries understood by the exit implementation.
    pub allowed_targets: Vec<String>,
    /// Target deny-list entries understood by the exit implementation.
    pub denied_targets: Vec<String>,
    /// Maximum concurrent circuits this exit wants to serve. `0` means unspecified.
    pub max_circuits: u32,
    /// Maximum streams per circuit. `0` means unspecified.
    pub max_streams_per_circuit: u32,
    /// Maximum bytes per minute. `0` means unspecified.
    pub max_bytes_per_minute: u64,
}

/// Descriptor fields covered by the onion-exit signature.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct OnionExitDescriptorBody {
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Services this exit is willing to expose.
    pub services: Vec<OnionExitService>,
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
    fn validate_signer(&self, session_sk: &SessionSk) -> CoreResult<()> {
        if self.public_key.did() != self.did || session_sk.account_did() != self.did {
            return Err(CoreError::InvalidMessage(
                "onion exit descriptor DID/public key/session mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        OnionExitDescriptorBodyRef {
            did: self.did,
            public_key: &self.public_key,
            node_type: &self.node_type,
            network_id: self.network_id,
            services: &self.services,
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

#[derive(Serialize)]
struct OnionExitDescriptorBodyRef<'a> {
    did: Did,
    public_key: &'a VerificationPublicKey,
    node_type: &'a OnlineNodeType,
    network_id: u32,
    services: &'a [OnionExitService],
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
    /// DID of the exit node/account.
    pub did: Did,
    /// Account public key corresponding to `did`.
    pub public_key: VerificationPublicKey,
    /// Runtime family of this exit node.
    pub node_type: OnlineNodeType,
    /// Network identifier.
    pub network_id: u32,
    /// Services this exit is willing to expose.
    pub services: Vec<OnionExitService>,
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
        body.validate_signer(session_sk)?;
        let signature = MessageVerification::new(&body.signing_data()?, session_sk)?;
        Ok(Self {
            did: body.did,
            public_key: body.public_key,
            node_type: body.node_type,
            network_id: body.network_id,
            services: body.services,
            policy: body.policy,
            started_at_ms: body.started_at_ms,
            heartbeat_at_ms: body.heartbeat_at_ms,
            expires_at_ms: body.expires_at_ms,
            version: body.version,
            signature,
        })
    }

    fn body_ref(&self) -> OnionExitDescriptorBodyRef<'_> {
        let Self {
            did,
            public_key,
            node_type,
            network_id,
            services,
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
            node_type,
            network_id: *network_id,
            services,
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

    /// Return whether this descriptor belongs to `network_id`.
    pub const fn matches_network(&self, network_id: u32) -> bool {
        self.network_id == network_id
    }

    /// Return whether this descriptor offers `service`.
    pub fn offers_service(&self, service: &str) -> bool {
        self.services
            .iter()
            .any(|candidate| candidate.has_name(service))
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

    /// Select the newest valid onion-exit descriptor per DID.
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

impl Encoder for OnionExitDescriptor {
    fn encode(&self) -> CoreResult<Encoded> {
        bincode::serialize(self)
            .map_err(CoreError::BincodeSerialize)?
            .encode()
    }
}

impl Decoder for OnionExitDescriptor {
    fn from_encoded(encoded: &Encoded) -> CoreResult<Self> {
        let data: Vec<u8> = encoded.decode()?;
        bincode::deserialize(&data).map_err(CoreError::BincodeDeserialize)
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

    /// Build this node's signed onion-exit descriptor at `now_ms`.
    pub fn descriptor_at(
        &self,
        context: &RegistrationContext<'_>,
        now_ms: u128,
    ) -> Result<OnionExitDescriptor> {
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did: context.did(),
                public_key: context.account_verification_pubkey()?,
                node_type: self.node_type.clone(),
                network_id: context.network_id(),
                services: self.services.clone(),
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

    /// Publish this node's signed onion-exit descriptor.
    pub async fn publish_descriptor(
        &self,
        context: &RegistrationContext<'_>,
    ) -> Result<OnionExitDescriptor> {
        let now_ms = get_epoch_ms();
        let descriptor = self.descriptor_at(context, now_ms)?;
        let encoded = descriptor.encode().map_err(Error::CoreError)?;
        self.publisher.publish(context, encoded).await?;
        Ok(descriptor)
    }

    /// Decode onion-exit descriptors from a DHT entry.
    pub fn descriptors_from_entry(
        entry: &rings_core::prelude::entry::Entry,
    ) -> Vec<OnionExitDescriptor> {
        entry
            .data
            .iter()
            .filter_map(|value| value.decode::<OnionExitDescriptor>().ok())
            .collect()
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
        self.publish_descriptor(context).await.map(|_| ())
    }
}

/// Route-building request for an onion circuit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRouteRequest {
    /// Exit service required by the route.
    pub service: String,
    /// Desired hop count including the exit. `0` uses [`DEFAULT_ONION_ROUTE_HOPS`].
    pub hop_count: usize,
    /// Whether a route may be shorter than `hop_count` when the network is too small.
    pub allow_short_paths: bool,
}

impl OnionRouteRequest {
    fn target_hop_count(&self) -> usize {
        if self.hop_count == 0 {
            DEFAULT_ONION_ROUTE_HOPS
        } else {
            self.hop_count
        }
    }
}

/// Selected onion route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRoute {
    /// Exit service requested by the route.
    pub service: String,
    /// Ordered DIDs, ending with the exit DID.
    pub hops: Vec<Did>,
    /// Signed descriptor for the selected exit.
    pub exit: OnionExitDescriptor,
}

impl OnionRoute {
    /// Return the selected exit DID.
    pub fn exit_did(&self) -> Did {
        self.exit.did
    }
}

/// Select an onion route from live presence and exit descriptors.
///
/// Invariant: the returned hop list contains no duplicate DID and always ends
/// in a descriptor from the exit registry.
pub fn select_onion_route(
    local: Did,
    network_id: u32,
    now_ms: u128,
    request: &OnionRouteRequest,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Result<OnionRoute> {
    let service = request.service.trim();
    if service.is_empty() {
        return Err(Error::OnionRouteError(
            "onion route service must not be empty".to_string(),
        ));
    }

    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let mut exits_by_did = eligible_exits(network_id, now_ms, service, exits)
        .into_iter()
        .map(|descriptor| (descriptor.did, descriptor))
        .collect::<BTreeMap<_, _>>();

    let exit_did = order_dids_by_quality(exits_by_did.keys().copied(), &quality_by_did)
        .into_iter()
        .find(|did| *did != local)
        .ok_or_else(|| {
            Error::OnionRouteError(format!("no live onion exit offers service {service:?}"))
        })?;
    let exit = exits_by_did.remove(&exit_did).ok_or_else(|| {
        Error::OnionRouteError("selected onion exit disappeared during route selection".to_string())
    })?;

    let relay_dids = eligible_relay_dids(network_id, now_ms, local, exit_did, online_nodes);
    let ordered_relays = order_dids_by_quality(relay_dids, &quality_by_did);
    let relay_hops_needed = request.target_hop_count().saturating_sub(1);
    let selected_relays = ordered_relays
        .into_iter()
        .take(relay_hops_needed)
        .collect::<Vec<_>>();

    if selected_relays.len() < relay_hops_needed && !request.allow_short_paths {
        return Err(Error::OnionRouteError(format!(
            "not enough relay candidates for {}-hop onion route",
            request.target_hop_count()
        )));
    }

    let mut hops = selected_relays;
    hops.push(exit_did);
    if has_duplicate_dids(&hops) {
        return Err(Error::OnionRouteError(
            "onion route selection produced duplicate DIDs".to_string(),
        ));
    }

    Ok(OnionRoute {
        service: service.to_string(),
        hops,
        exit,
    })
}

fn eligible_exits(
    network_id: u32,
    now_ms: u128,
    service: &str,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
) -> Vec<OnionExitDescriptor> {
    OnionExitDescriptor::latest_valid_by_did(exits, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_network(network_id))
        .filter(|descriptor| descriptor.offers_service(service))
        .collect()
}

fn eligible_relay_dids(
    network_id: u32,
    now_ms: u128,
    local: Did,
    exit: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> BTreeSet<Did> {
    OnlineNodeDescriptor::latest_valid_by_did(online_nodes, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_network(network_id))
        .map(|descriptor| descriptor.did)
        .filter(|did| *did != local && *did != exit)
        .collect()
}

fn order_dids_by_quality(
    dids: impl IntoIterator<Item = Did>,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
) -> Vec<Did> {
    order_peers_by_quality(dids.into_iter().map(|did| {
        (
            did,
            quality_by_did
                .get(&did)
                .copied()
                .unwrap_or(PeerQuality::Unknown),
        )
    }))
}

fn has_duplicate_dids(hops: &[Did]) -> bool {
    let mut seen = BTreeSet::new();
    hops.iter().any(|did| !seen.insert(*did))
}

#[cfg(test)]
mod tests {
    use rings_core::ecc::SecretKey;
    use rings_core::session::SessionSk;

    use crate::online::OnlineNodeDescriptorBody;

    use super::*;

    fn service(name: &str) -> OnionExitService {
        OnionExitService {
            name: name.to_string(),
            transport: OnionExitTransport::Tcp,
        }
    }

    fn signed_exit_at(
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
    ) -> CoreResult<OnionExitDescriptor> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key: session_sk.session().account_verification_pubkey()?,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy {
                    allowed_targets: vec!["example.com:443".to_string()],
                    denied_targets: vec![],
                    max_circuits: 16,
                    max_streams_per_circuit: 4,
                    max_bytes_per_minute: 1024,
                },
                started_at_ms: 1,
                heartbeat_at_ms,
                expires_at_ms,
                version: "test".to_string(),
            },
            &session_sk,
        )
    }

    fn online_node_at(
        session_sk: &SessionSk,
        heartbeat_at_ms: u128,
        expires_at_ms: u128,
    ) -> CoreResult<OnlineNodeDescriptor> {
        OnlineNodeDescriptor::new_signed(
            OnlineNodeDescriptorBody {
                did: session_sk.account_did(),
                public_key: session_sk.session().account_verification_pubkey()?,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                capabilities: vec![],
                endpoint_hint: None,
                started_at_ms: 1,
                heartbeat_at_ms,
                expires_at_ms,
                version: "test".to_string(),
            },
            session_sk,
        )
    }

    fn node_key() -> CoreResult<SessionSk> {
        SessionSk::new_with_seckey(&SecretKey::random())
    }

    #[test]
    fn exit_descriptor_signature_covers_policy() -> CoreResult<()> {
        let mut descriptor = signed_exit_at(20, 100)?;
        assert!(descriptor.verify_signature());

        descriptor.policy.max_circuits = 32;

        assert!(!descriptor.verify_signature());
        Ok(())
    }

    #[test]
    fn latest_valid_by_did_filters_expired_and_keeps_newest() -> CoreResult<()> {
        let key = SecretKey::random();
        let session_sk = SessionSk::new_with_seckey(&key)?;
        let did = session_sk.account_did();
        let public_key = session_sk.session().account_verification_pubkey()?;

        let older = OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key: public_key.clone(),
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy::default(),
                started_at_ms: 1,
                heartbeat_at_ms: 10,
                expires_at_ms: 100,
                version: "old".to_string(),
            },
            &session_sk,
        )?;
        let newer = OnionExitDescriptor::new_signed(
            OnionExitDescriptorBody {
                did,
                public_key,
                node_type: OnlineNodeType::Native,
                network_id: 1,
                services: vec![service("web")],
                policy: OnionExitPolicy::default(),
                started_at_ms: 1,
                heartbeat_at_ms: 20,
                expires_at_ms: 100,
                version: "new".to_string(),
            },
            &session_sk,
        )?;
        let other_live = signed_exit_at(25, 100)?;
        let expired = signed_exit_at(30, 40)?;

        let descriptors = OnionExitDescriptor::latest_valid_by_did(
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

        let with_expired = OnionExitDescriptor::latest_valid_by_did(
            vec![older, newer, other_live, expired],
            50,
            true,
        );
        assert_eq!(with_expired.len(), 3);
        Ok(())
    }

    #[test]
    fn route_builder_uses_presence_relays_and_exit_registry() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let first_relay = node_key().map_err(Error::CoreError)?;
        let second_relay = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let online = vec![
            online_node_at(&first_relay, 20, 100).map_err(Error::CoreError)?,
            online_node_at(&second_relay, 20, 100).map_err(Error::CoreError)?,
        ];
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 3,
            allow_short_paths: false,
        };

        let route = select_onion_route(
            local,
            1,
            50,
            &request,
            online,
            vec![exit.clone()],
            Vec::new(),
        )?;

        assert_eq!(route.hops.len(), 3);
        assert_eq!(route.exit_did(), exit.did);
        assert_eq!(route.hops.last().copied(), Some(exit.did));
        assert_ne!(route.hops.first().copied(), Some(exit.did));
        Ok(())
    }

    #[test]
    fn route_builder_rejects_too_short_production_route() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let relay = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 3,
            allow_short_paths: false,
        };

        let result = select_onion_route(
            local,
            1,
            50,
            &request,
            vec![online_node_at(&relay, 20, 100).map_err(Error::CoreError)?],
            vec![exit],
            Vec::new(),
        );

        assert!(
            matches!(result, Err(Error::OnionRouteError(message)) if message.contains("not enough relay"))
        );
        Ok(())
    }

    #[test]
    fn route_builder_orders_relays_by_quality() -> Result<()> {
        let local = node_key().map_err(Error::CoreError)?.account_did();
        let degraded = node_key().map_err(Error::CoreError)?;
        let healthy = node_key().map_err(Error::CoreError)?;
        let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
        let online = vec![
            online_node_at(&degraded, 20, 100).map_err(Error::CoreError)?,
            online_node_at(&healthy, 20, 100).map_err(Error::CoreError)?,
        ];
        let request = OnionRouteRequest {
            service: "web".to_string(),
            hop_count: 2,
            allow_short_paths: false,
        };

        let route = select_onion_route(
            local,
            1,
            50,
            &request,
            online,
            vec![exit],
            vec![
                (degraded.account_did(), PeerQuality::Degraded),
                (healthy.account_did(), PeerQuality::Healthy),
            ],
        )?;

        assert_eq!(route.hops.first().copied(), Some(healthy.account_did()));
        Ok(())
    }
}
