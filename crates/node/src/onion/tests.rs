use std::collections::VecDeque;

use rings_core::ecc::SecretKey;
use rings_core::measure::PeerQuality;
use rings_core::message::Encoder;
use rings_core::prelude::entry;
use rings_core::session::SessionSk;

use super::route::RouteEntropy;
use super::*;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;

fn service(name: &str) -> OnionExitService {
    OnionExitService::new(name, OnionExitTransport::Tcp).expect("valid test service")
}

fn signed_exit_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> Result<OnionExitDescriptor> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(Error::CoreError)?;
    signed_exit_for_session_at(
        &session_sk,
        service("web"),
        heartbeat_at_ms,
        expires_at_ms,
        "test",
    )
}

fn signed_exit_for_session_at(
    session_sk: &SessionSk,
    service: OnionExitService,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
    version: &str,
) -> Result<OnionExitDescriptor> {
    let did = session_sk.account_did();
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key: session_sk
                .session()
                .account_verification_pubkey()
                .map_err(Error::CoreError)?,
            session_public_key: session_sk.session_public_key(),
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service,
            policy: OnionExitPolicy {
                allowed_targets: vec![OnionExitTarget::parse("example.com:443")?],
                denied_targets: vec![],
                max_circuits: 16,
                max_streams_per_circuit: 4,
                max_bytes_per_minute: 1024,
            },
            started_at_ms: 1,
            heartbeat_at_ms,
            expires_at_ms,
            version: version.to_string(),
        },
        session_sk,
    )
    .map_err(Error::CoreError)
}

fn online_node_at(
    session_sk: &SessionSk,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
) -> CoreResult<OnlineNodeDescriptor> {
    online_node_at_with_capabilities(
        session_sk,
        heartbeat_at_ms,
        expires_at_ms,
        vec![ONION_RELAY_CAPABILITY.to_string()],
    )
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
            session_public_key: session_sk.session_public_key(),
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
    assert!(!OnionExitService::new("https", OnionExitTransport::Tcp)
        .expect("valid service")
        .matches_route_service("https"));
    assert!(OnionExitService::new("custom", OnionExitTransport::Tcp)
        .expect("valid service")
        .matches_route_service("custom"));
}

#[test]
fn onion_exit_service_name_is_validated_and_canonicalized() -> Result<()> {
    let service = OnionExitService::new("WeB-Api.1", OnionExitTransport::Tcp)?;

    assert_eq!(service.name.as_str(), "web-api.1");
    assert!(OnionExitService::new("", OnionExitTransport::Tcp).is_err());
    assert!(OnionExitService::new(" web", OnionExitTransport::Tcp).is_err());
    assert!(OnionExitService::new("web!", OnionExitTransport::Tcp).is_err());
    Ok(())
}

#[test]
fn default_exit_policy_is_closed() -> Result<()> {
    let policy = OnionExitPolicy::default();
    let target = OnionExitTarget::parse("example.com:443")?;

    assert!(policy.is_closed());
    assert!(!policy.allows_target(&target));
    assert!(matches!(
        policy.validate_targets(),
        Err(Error::InvalidConfig(message)) if message.contains("allowed target")
    ));
    Ok(())
}

#[test]
fn exit_policy_allow_list_controls_targets() -> Result<()> {
    let policy = OnionExitPolicy::from_target_strings(
        vec![
            "Example.COM.:443".to_string(),
            "API.example.com:443".to_string(),
        ],
        vec!["api.example.com:443".to_string()],
    )?;
    let example = OnionExitTarget::parse("example.com:443")?;
    let api = OnionExitTarget::parse("api.example.com:443")?;
    let other = OnionExitTarget::parse("other.example.com:443")?;

    assert!(!policy.is_closed());
    assert!(policy.allows_target(&example));
    assert!(!policy.allows_target(&api));
    assert!(!policy.allows_target(&other));
    Ok(())
}

#[test]
fn exit_policy_rejects_invalid_target_entries() {
    assert!(matches!(
        OnionExitPolicy::from_target_strings(vec!["example.com".to_string()], vec![]),
        Err(Error::InvalidConfig(message)) if message.contains("expected host:port")
    ));

    assert!(matches!(
        OnionExitPolicy::from_target_strings(
            vec!["example.com:443".to_string()],
            vec!["blocked.example.com".to_string()]
        ),
        Err(Error::InvalidConfig(message)) if message.contains("expected host:port")
    ));
}

#[test]
fn exit_descriptor_signature_covers_policy() -> Result<()> {
    let mut descriptor = signed_exit_at(20, 100)?;
    assert!(descriptor.verify_signature());

    descriptor.policy.max_circuits = 32;

    assert!(!descriptor.verify_signature());
    Ok(())
}

#[test]
fn exit_descriptor_signature_covers_schema_version() -> Result<()> {
    let mut descriptor = signed_exit_at(20, 100)?;
    assert_eq!(
        descriptor.schema_version,
        ONION_EXIT_DESCRIPTOR_SCHEMA_VERSION
    );
    assert!(descriptor.verify_signature());

    descriptor.schema_version = descriptor.schema_version.saturating_add(1);

    assert!(!descriptor.verify_signature());
    Ok(())
}

#[test]
fn exit_registry_decode_reports_rejected_schema_values() -> Result<()> {
    let valid = signed_exit_at(20, 100)?;
    let mut unsupported = signed_exit_at(21, 100)?;
    unsupported.schema_version = unsupported.schema_version.saturating_add(1);
    let data = vec![
        valid.encode().map_err(Error::CoreError)?,
        unsupported.encode().map_err(Error::CoreError)?,
    ];
    let entry = entry::Entry::new(
        entry::Entry::gen_did(ONION_EXITS_TOPIC)?,
        data,
        entry::EntryKind::Data,
    );

    let report = OnionExitRegistration::decode_descriptors_from_entry(&entry);

    assert_eq!(report.descriptors, vec![valid]);
    assert_eq!(report.rejected_values, 1);
    Ok(())
}

#[test]
fn latest_valid_by_service_did_filters_expired_and_keeps_newest() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(Error::CoreError)?;
    let did = session_sk.account_did();
    let public_key = session_sk
        .session()
        .account_verification_pubkey()
        .map_err(Error::CoreError)?;

    let older = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key: public_key.clone(),
            session_public_key: session_sk.session_public_key(),
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: service("web"),
            policy: OnionExitPolicy::default(),
            started_at_ms: 1,
            heartbeat_at_ms: 10,
            expires_at_ms: 100,
            version: "old".to_string(),
        },
        &session_sk,
    )
    .map_err(Error::CoreError)?;
    let newer = OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did,
            public_key,
            session_public_key: session_sk.session_public_key(),
            node_type: OnlineNodeType::Native,
            network_id: 1,
            service: service("web"),
            policy: OnionExitPolicy::default(),
            started_at_ms: 1,
            heartbeat_at_ms: 20,
            expires_at_ms: 100,
            version: "new".to_string(),
        },
        &session_sk,
    )
    .map_err(Error::CoreError)?;
    let other_live = signed_exit_at(25, 100)?;
    let expired = signed_exit_at(30, 40)?;

    let descriptors = OnionExitDescriptor::latest_valid_by_service_did(
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

    let with_expired = OnionExitDescriptor::latest_valid_by_service_did(
        vec![older, newer, other_live, expired],
        50,
        true,
    );
    assert_eq!(with_expired.len(), 3);
    Ok(())
}

#[test]
fn latest_valid_by_service_did_preserves_same_did_distinct_services() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(Error::CoreError)?;
    let old_tcp =
        signed_exit_for_session_at(&session_sk, OnionExitService::tcp(), 10, 100, "tcp-old")?;
    let new_tcp =
        signed_exit_for_session_at(&session_sk, OnionExitService::tcp(), 20, 100, "tcp-new")?;
    let https =
        signed_exit_for_session_at(&session_sk, OnionExitService::https(), 15, 100, "https")?;
    let wrong_https_transport = signed_exit_for_session_at(
        &session_sk,
        OnionExitService::new("https", OnionExitTransport::Tcp)?,
        25,
        100,
        "https-wrong-transport",
    )?;

    let descriptors = OnionExitDescriptor::latest_valid_by_service_did(
        vec![
            old_tcp,
            new_tcp.clone(),
            https.clone(),
            wrong_https_transport.clone(),
        ],
        50,
        false,
    );

    assert_eq!(descriptors.len(), 3);
    assert!(descriptors.iter().any(|descriptor| descriptor == &new_tcp));
    assert!(descriptors.iter().any(|descriptor| descriptor == &https));
    assert!(descriptors
        .iter()
        .any(|descriptor| descriptor == &wrong_https_transport));
    Ok(())
}

#[test]
fn route_builder_uses_presence_relays_and_exit_registry() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let first_relay = node_key().map_err(Error::CoreError)?;
    let second_relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
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

    assert_eq!(route.hops().len(), 3);
    assert_eq!(route.exit_did(), exit.did);
    assert_eq!(route.hops().last().copied(), Some(exit.did));
    assert_ne!(route.hops().first().copied(), Some(exit.did));
    Ok(())
}

#[test]
fn route_builder_rejects_too_short_production_route() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
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

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NotEnoughRelays {
            hop_count: 3
        }))
    ));
    Ok(())
}

#[test]
fn route_builder_rejects_nodes_without_relay_capability() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
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

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NotEnoughRelays {
            hop_count: 2
        }))
    ));
    Ok(())
}

#[test]
fn route_builder_samples_relays_by_quality_weight() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let degraded = node_key().map_err(Error::CoreError)?;
    let healthy = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = OnionRouteRequest {
        service: "web".to_string(),
        hop_count: 2,
        allow_short_paths: false,
    };
    let candidates = OnionRouteCandidates {
        relays: vec![
            OnionRouteHop::new(degraded.account_did(), degraded.session_public_key()),
            OnionRouteHop::new(healthy.account_did(), healthy.session_public_key()),
        ],
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

    assert_eq!(route.hops().first().copied(), Some(healthy.account_did()));
    assert_ne!(route.hops().first().copied(), Some(local));
    Ok(())
}

#[test]
fn route_builder_entropy_can_select_second_unknown_relay() -> Result<()> {
    let first = node_key().map_err(Error::CoreError)?;
    let second = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = OnionRouteRequest {
        service: "web".to_string(),
        hop_count: 2,
        allow_short_paths: false,
    };
    let mut relay_hops = vec![
        OnionRouteHop::new(first.account_did(), first.session_public_key()),
        OnionRouteHop::new(second.account_did(), second.session_public_key()),
    ];
    relay_hops.sort_by_key(|hop| hop.did);
    let second_sorted = relay_hops
        .get(1)
        .map(|hop| hop.did)
        .ok_or(Error::OnionRouteError(OnionRouteError::MissingTestRelay))?;
    let candidates = OnionRouteCandidates {
        relays: relay_hops,
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 4]);

    let route = select_onion_route_from_candidates(&request, candidates, Vec::new(), &mut entropy)?;

    assert_eq!(route.hops().first().copied(), Some(second_sorted));
    Ok(())
}
