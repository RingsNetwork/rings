use std::collections::VecDeque;

use rings_core::ecc::SecretKey;
use rings_core::error::Result as CoreResult;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::session::SessionSk;

use super::super::*;
use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTarget;
use crate::onion::OnionExitTransport;
use crate::onion::OnionRouteError;
use crate::onion::ONION_RELAY_CAPABILITY;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;

fn service(name: &str) -> OnionExitService {
    OnionExitService::new(name, OnionExitTransport::Tcp).expect("valid test service")
}

fn signed_exit_at(heartbeat_at_ms: u128, expires_at_ms: u128) -> Result<OnionExitDescriptor> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key).map_err(Error::CoreError)?;
    signed_exit_for_session_at(&session_sk, heartbeat_at_ms, expires_at_ms)
}

fn signed_exit_for_session_at(
    session_sk: &SessionSk,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
) -> Result<OnionExitDescriptor> {
    signed_exit_for_session_network_at(session_sk, 1, heartbeat_at_ms, expires_at_ms)
}

fn signed_exit_for_session_network_at(
    session_sk: &SessionSk,
    network_id: u32,
    heartbeat_at_ms: u128,
    expires_at_ms: u128,
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
            network_id,
            service: service("web"),
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
            version: "test".to_string(),
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
    online_node_at_with_network_and_capabilities(
        session_sk,
        1,
        heartbeat_at_ms,
        expires_at_ms,
        capabilities,
    )
}

fn online_node_at_with_network_and_capabilities(
    session_sk: &SessionSk,
    network_id: u32,
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
            network_id,
            storage_redundancy: DATA_REDUNDANT,
            dht_virtual_nodes: 0,
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

fn route_request(
    service: &str,
    hop_count: usize,
    allow_short_paths: bool,
) -> Result<OnionRouteRequest> {
    OnionRouteRequest::new(service, hop_count, allow_short_paths)
}

fn test_dht_protocol() -> DhtProtocolMode {
    DhtProtocolMode::new(1, DATA_REDUNDANT, 0)
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
fn test_route_builder_uses_presence_relays_and_exit_registry() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let first_relay = node_key().map_err(Error::CoreError)?;
    let second_relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let online = vec![
        online_node_at(&first_relay, 20, 100).map_err(Error::CoreError)?,
        online_node_at(&second_relay, 20, 100).map_err(Error::CoreError)?,
    ];
    let request = route_request("web", 3, false)?;

    let route = select_onion_route(
        local,
        test_dht_protocol(),
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
fn test_route_builder_canonicalizes_service_before_constructing_route() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("WeB", 1, false)?;

    let route = select_onion_route(
        local,
        test_dht_protocol(),
        50,
        &request,
        Vec::new(),
        vec![exit],
        Vec::new(),
    )?;

    assert_eq!(route.service(), "web");
    Ok(())
}

#[test]
fn test_directory_candidates_reject_expired_remote_descriptors() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 40)?;
    let candidates = OnionRouteCandidates::from_validated_descriptors(
        local,
        test_dht_protocol(),
        50,
        route_request("web", 1, false)?.service_name(),
        vec![online_node_at(&relay, 20, 40).map_err(Error::CoreError)?],
        vec![exit],
    );

    assert!(candidates.relays.is_empty());
    assert!(candidates.exits.is_empty());
    Ok(())
}

#[test]
fn test_directory_candidates_reject_foreign_network_descriptors() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit_key = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_for_session_network_at(&exit_key, 2, 20, 100)?;
    let candidates = OnionRouteCandidates::from_validated_descriptors(
        local,
        test_dht_protocol(),
        50,
        route_request("web", 1, false)?.service_name(),
        vec![
            online_node_at_with_network_and_capabilities(&relay, 2, 20, 100, vec![
                ONION_RELAY_CAPABILITY.to_string(),
            ])
            .map_err(Error::CoreError)?,
        ],
        vec![exit],
    );

    assert!(candidates.relays.is_empty());
    assert!(candidates.exits.is_empty());
    Ok(())
}

#[test]
fn test_route_builder_rejects_too_short_production_route() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("web", 3, false)?;

    let result = select_onion_route(
        local,
        test_dht_protocol(),
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
fn test_route_builder_rejects_nodes_without_relay_capability() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let relay = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("web", 2, false)?;

    let result = select_onion_route(
        local,
        test_dht_protocol(),
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
fn test_route_builder_reports_no_live_exit_before_first_hop_filter() -> Result<()> {
    let request = route_request("web", 1, false)?;
    let candidates = OnionRouteCandidates {
        relays: Vec::new(),
        exits: Vec::new(),
    };
    let mut entropy = FixedEntropy::new([0]);

    let result = select_onion_route_from_candidates_with_first_hop(
        &request,
        candidates,
        Vec::new(),
        &mut entropy,
        |_| false,
    );

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service
        })) if service == "web"
    ));
    Ok(())
}

#[test]
fn test_route_builder_samples_relays_by_quality_weight() -> Result<()> {
    let local = node_key().map_err(Error::CoreError)?.account_did();
    let degraded = node_key().map_err(Error::CoreError)?;
    let healthy = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("web", 2, false)?;
    let candidates = OnionRouteCandidates {
        relays: vec![
            OnionRouteHop::new(degraded.account_did(), degraded.session_public_key()),
            OnionRouteHop::new(healthy.account_did(), healthy.session_public_key()),
        ],
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([1, 0]);

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
fn test_route_builder_entropy_can_select_second_unknown_relay() -> Result<()> {
    let first = node_key().map_err(Error::CoreError)?;
    let second = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("web", 2, false)?;
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
    let mut entropy = FixedEntropy::new([4, 0]);

    let route = select_onion_route_from_candidates(&request, candidates, Vec::new(), &mut entropy)?;

    assert_eq!(route.hops().first().copied(), Some(second_sorted));
    Ok(())
}

#[test]
fn test_route_builder_first_hop_filter_preserves_remote_later_relays() -> Result<()> {
    let direct = node_key().map_err(Error::CoreError)?;
    let remote = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let direct_did = direct.account_did();
    let remote_did = remote.account_did();
    let request = route_request("web", 3, false)?;
    let candidates = OnionRouteCandidates {
        relays: vec![
            OnionRouteHop::new(remote_did, remote.session_public_key()),
            OnionRouteHop::new(direct_did, direct.session_public_key()),
        ],
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 0, 0]);

    let route = select_onion_route_from_candidates_with_first_hop(
        &request,
        candidates,
        Vec::new(),
        &mut entropy,
        |did| did == direct_did,
    )?;

    assert_eq!(route.hops().len(), 3);
    assert_eq!(route.hops().first().copied(), Some(direct_did));
    assert!(route.hops().contains(&remote_did));
    Ok(())
}

#[test]
fn test_route_builder_does_not_consume_only_direct_relay_as_exit_first() -> Result<()> {
    let direct = node_key().map_err(Error::CoreError)?;
    let direct_exit = signed_exit_for_session_at(&direct, 20, 100)?;
    let remote_exit = signed_exit_at(21, 100)?;
    let direct_did = direct.account_did();
    let request = route_request("web", 2, false)?;
    let candidates = OnionRouteCandidates {
        relays: vec![OnionRouteHop::new(direct_did, direct.session_public_key())],
        exits: vec![direct_exit, remote_exit.clone()],
    };
    let mut entropy = FixedEntropy::new([0, 0]);

    let route = select_onion_route_from_candidates_with_first_hop(
        &request,
        candidates,
        Vec::new(),
        &mut entropy,
        |did| did == direct_did,
    )?;

    assert_eq!(route.hops(), &[direct_did, remote_exit.did]);
    Ok(())
}

#[test]
fn test_route_builder_rejects_route_without_permitted_first_hop() -> Result<()> {
    let permitted = node_key().map_err(Error::CoreError)?.account_did();
    let remote = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let request = route_request("web", 2, false)?;
    let candidates = OnionRouteCandidates {
        relays: vec![OnionRouteHop::new(
            remote.account_did(),
            remote.session_public_key(),
        )],
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 0]);

    let result = select_onion_route_from_candidates_with_first_hop(
        &request,
        candidates,
        Vec::new(),
        &mut entropy,
        |did| did == permitted,
    );

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))
    ));
    Ok(())
}

#[test]
fn test_route_builder_shortens_to_permitted_exit_when_no_first_relay_is_allowed() -> Result<()> {
    let remote = node_key().map_err(Error::CoreError)?;
    let exit = signed_exit_at(20, 100)?;
    let exit_did = exit.did;
    let request = route_request("web", 3, true)?;
    let candidates = OnionRouteCandidates {
        relays: vec![OnionRouteHop::new(
            remote.account_did(),
            remote.session_public_key(),
        )],
        exits: vec![exit],
    };
    let mut entropy = FixedEntropy::new([0, 0]);

    let route = select_onion_route_from_candidates_with_first_hop(
        &request,
        candidates,
        Vec::new(),
        &mut entropy,
        |did| did == exit_did,
    )?;

    assert_eq!(route.hops().len(), 1);
    assert_eq!(route.hops().first().copied(), Some(exit_did));
    Ok(())
}
