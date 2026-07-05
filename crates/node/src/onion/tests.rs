use std::collections::VecDeque;

use rings_core::ecc::SecretKey;
use rings_core::measure::PeerQuality;
use rings_core::session::SessionSk;

use super::route::RouteEntropy;
use super::*;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;

fn service(name: &str) -> OnionExitService {
    OnionExitService {
        name: name.to_string(),
        transport: OnionExitTransport::Tcp,
    }
}

fn signed_exit_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> CoreResult<OnionExitDescriptor> {
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
    online_node_at_with_capabilities(session_sk, heartbeat_at_ms, expires_at_ms, vec![
        ONION_RELAY_CAPABILITY.to_string(),
    ])
}

fn online_node_at_with_capabilities(
    session_sk: &SessionSk,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    capabilities: Vec<String>,
) -> CoreResult<OnlineNodeDescriptor> {
    OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: session_sk.account_did(),
            public_key: session_sk.session().account_verification_pubkey()?,
            node_type: OnlineNodeType::Native,
            network_id: 1,
            capabilities,
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

struct FixedEntropy {
    values: VecDeque<u64>,
}

impl FixedEntropy {
    fn new(values: impl IntoIterator<Item = u64>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl RouteEntropy for FixedEntropy {
    fn next_u64(&mut self) -> u64 {
        self.values.pop_front().unwrap_or(0)
    }
}

#[test]
fn default_exit_services_include_native_tcp_only() {
    assert_eq!(default_onion_exit_services(), vec![OnionExitService::tcp()]);
    assert_eq!(https_onion_exit_services(), vec![OnionExitService::https()]);
}

#[test]
fn reserved_service_name_requires_reserved_transport_for_routes() {
    assert!(OnionExitService::https().matches_route_service("https"));
    assert!(!OnionExitService::new("https", OnionExitTransport::Tcp).matches_route_service("https"));
    assert!(
        OnionExitService::new("custom", OnionExitTransport::Tcp).matches_route_service("custom")
    );
}

#[test]
fn default_exit_policy_is_closed() {
    let policy = OnionExitPolicy::default();
    assert!(policy.is_closed());
    assert!(!policy.allows_target("example.com:443"));
    assert!(matches!(
        policy.validate_targets(),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));
}

#[test]
fn exit_policy_allow_list_controls_targets() {
    let policy = OnionExitPolicy {
        allowed_targets: vec![
            "Example.COM.:443".to_string(),
            "API.example.com:443".to_string(),
        ],
        denied_targets: vec!["api.example.com:443".to_string()],
        max_circuits: 0,
        max_streams_per_circuit: 0,
        max_bytes_per_minute: 0,
    };

    assert!(!policy.is_closed());
    assert!(policy.allows_target("example.com:443"));
    assert!(!policy.allows_target("api.example.com:443"));
    assert!(!policy.allows_target("other.example.com:443"));
    assert!(!policy.allows_target(""));
}

#[test]
fn exit_policy_rejects_invalid_target_entries() {
    let invalid_allowed = OnionExitPolicy {
        allowed_targets: vec!["example.com".to_string()],
        ..OnionExitPolicy::default()
    };
    assert!(matches!(
        invalid_allowed.validate_targets(),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));

    let invalid_denied = OnionExitPolicy {
        allowed_targets: vec!["example.com:443".to_string()],
        denied_targets: vec!["blocked.example.com".to_string()],
        ..OnionExitPolicy::default()
    };
    assert!(matches!(
        invalid_denied.validate_targets(),
        Err(Error::InvalidConfig(message)) if message.contains("denied target")
    ));
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

    let with_expired =
        OnionExitDescriptor::latest_valid_by_did(vec![older, newer, other_live, expired], 50, true);
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
fn route_builder_rejects_nodes_without_relay_capability() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
    let request = OnionRouteRequest {
        service: "web".to_string(),
        hop_count: 2,
        allow_short_paths: false,
    };

    let result = select_onion_route(
        local,
        1,
        50,
        &request,
        vec![online_node_at_with_capabilities(&relay, 20, 100, vec![]).map_err(Error::CoreError)?],
        vec![exit],
        Vec::new(),
    );

    assert!(
        matches!(result, Err(Error::OnionRouteError(message)) if message.contains("not enough relay"))
    );
    Ok(())
}

#[test]
fn route_builder_samples_relays_by_quality_weight() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let degraded = node_key().map_err(Error::CoreError)?;
    let healthy = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
    let request = OnionRouteRequest {
        service: "web".to_string(),
        hop_count: 2,
        allow_short_paths: false,
    };
    let candidates = OnionRouteCandidates {
        relays: vec![degraded.account_did(), healthy.account_did()],
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 1]);

    let route = select_onion_route_from_candidates(
        &request,
        candidates,
        vec![
            (degraded.account_did(), PeerQuality::Degraded),
            (healthy.account_did(), PeerQuality::Healthy),
        ],
        &mut entropy,
    )?;

    assert_eq!(route.hops.first().copied(), Some(healthy.account_did()));
    assert_ne!(route.hops.first().copied(), Some(local));
    Ok(())
}

#[test]
fn route_builder_entropy_can_select_second_unknown_relay() -> Result<()> {
    let first = node_key().map_err(Error::CoreError)?.account_did();
    let second = node_key().map_err(Error::CoreError)?.account_did();
    let exit = signed_exit_at(20, 100).map_err(Error::CoreError)?;
    let request = OnionRouteRequest {
        service: "web".to_string(),
        hop_count: 2,
        allow_short_paths: false,
    };
    let mut relay_dids = vec![first, second];
    relay_dids.sort();
    let second_sorted = relay_dids
        .get(1)
        .copied()
        .ok_or_else(|| Error::OnionRouteError("missing second relay".to_string()))?;
    let candidates = OnionRouteCandidates {
        relays: relay_dids,
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 4]);

    let route = select_onion_route_from_candidates(&request, candidates, Vec::new(), &mut entropy)?;

    assert_eq!(route.hops.first().copied(), Some(second_sorted));
    Ok(())
}
