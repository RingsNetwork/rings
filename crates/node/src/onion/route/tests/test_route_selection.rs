use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::ops::RangeInclusive;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::message::MessageSigner;

use super::super::*;
use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitDescriptor;
use crate::onion::OnionExitDescriptorBody;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitTarget;
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
use crate::onion::MAX_ONION_LOOP_SYMBOLS;
use crate::online::OnlineNodeCapabilities;
use crate::online::OnlineNodeDescriptor;
use crate::online::OnlineNodeDescriptorBody;
use crate::online::OnlineNodeType;

/// Network of every fixture unless a test names another.
const TEST_NETWORK_ID: u32 = 1;
/// Current process epoch of every fixture node.
const TEST_PROCESS_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([29; 16]);
/// Process epoch of an earlier process of a fixture node.
const STALE_PROCESS_EPOCH: OnionProcessEpoch = OnionProcessEpoch::new([31; 16]);
/// Clock reading at which fixtures are live.
const NOW_MS: u128 = 50;
/// Heartbeat of every live fixture descriptor.
const HEARTBEAT_MS: u128 = 20;
/// Expiry of every live fixture descriptor.
const EXPIRES_MS: u128 = 100;

/// The node whose account secret key is `[seed; 32]`: fixed DIDs make every DID-ordered draw
/// reproducible. `seed` is non-zero.
fn node_key(seed: u8) -> Result<DelegateeKey> {
    let secret =
        SecretKey::try_from(format!("{seed:02x}").repeat(32).as_str()).map_err(Error::CoreError)?;
    DelegateeKey::new_with_seckey(&secret).map_err(Error::CoreError)
}

/// The nodes of `seeds`.
fn node_keys(seeds: RangeInclusive<u8>) -> Result<Vec<DelegateeKey>> {
    seeds.map(node_key).collect()
}

/// `keys` in DID order, the order of every weighted draw.
fn in_did_order(keys: &[DelegateeKey]) -> Vec<&DelegateeKey> {
    let mut ordered = keys.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|key| key.delegator_did());
    ordered
}

/// The relay hop of `key` at `epoch`.
fn hop(key: &DelegateeKey, epoch: OnionProcessEpoch) -> OnionRouteHop {
    OnionRouteHop::new(key.delegator_did(), key.delegatee_public_key(), epoch)
}

/// A signed `tcp` registration of `key` at `epoch` on `network_id`, live until `expires_at_ms`.
fn exit_descriptor(
    key: &DelegateeKey,
    epoch: OnionProcessEpoch,
    network_id: u32,
    expires_at_ms: u128,
) -> Result<OnionExitDescriptor> {
    OnionExitDescriptor::new_signed(
        OnionExitDescriptorBody {
            did: key.delegator_did(),
            public_key: key
                .delegation()
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: key.delegatee_public_key(),
            process_epoch: epoch,
            node_type: OnlineNodeType::Native,
            network_id,
            service: OnionServiceName::tcp(),
            policy: OnionExitPolicy {
                allowed_targets: vec![OnionExitTarget::parse("example.com:443")?],
                denied_targets: vec![],
                max_circuits: 16,
                max_streams_per_circuit: 4,
                max_bytes_per_minute: 1024,
            },
            started_at_ms: 1,
            heartbeat_at_ms: HEARTBEAT_MS,
            expires_at_ms,
            version: "test".to_string(),
        },
        MessageSigner::new(key, network_id),
    )
    .map_err(Error::CoreError)
}

/// A live `tcp` registration of `key` at the current epoch.
fn live_exit(key: &DelegateeKey) -> Result<OnionExitDescriptor> {
    exit_descriptor(key, TEST_PROCESS_EPOCH, TEST_NETWORK_ID, EXPIRES_MS)
}

/// A signed online-node descriptor of `key` with `capabilities`, live until `expires_at_ms`.
fn online_descriptor(
    key: &DelegateeKey,
    capabilities: OnlineNodeCapabilities,
    network_id: u32,
    expires_at_ms: u128,
) -> Result<OnlineNodeDescriptor> {
    OnlineNodeDescriptor::new_signed(
        OnlineNodeDescriptorBody {
            did: key.delegator_did(),
            public_key: key
                .delegation()
                .delegator_verification_pubkey()
                .map_err(Error::CoreError)?,
            delegatee_public_key: key.delegatee_public_key(),
            node_type: OnlineNodeType::Native,
            network_id,
            storage_redundancy: DATA_REDUNDANT,
            dht_virtual_nodes: 0,
            capabilities,
            endpoint_hint: None,
            started_at_ms: 1,
            heartbeat_at_ms: HEARTBEAT_MS,
            expires_at_ms,
            version: "test".to_string(),
        },
        MessageSigner::new(key, network_id),
    )
    .map_err(Error::CoreError)
}

/// A live online-node descriptor of `key` registering `relay` at the current epoch.
fn live_relay(key: &DelegateeKey) -> Result<OnlineNodeDescriptor> {
    online_descriptor(
        key,
        OnlineNodeCapabilities::onion_relay(TEST_PROCESS_EPOCH),
        TEST_NETWORK_ID,
        EXPIRES_MS,
    )
}

/// The request for a loop over the `tcp` symbol.
fn tcp_request() -> OnionRouteRequest {
    OnionRouteRequest::from_service_name(OnionServiceName::tcp())
}

/// The local DHT protocol mode of every fixture.
fn test_dht_protocol() -> DhtProtocolMode {
    DhtProtocolMode::new(TEST_NETWORK_ID, DATA_REDUNDANT, 0)
}

/// Admit `online_nodes` and `exits` through the directory boundary for the `tcp` symbol.
fn tcp_candidates(
    local: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
) -> OnionRouteCandidates {
    OnionRouteCandidates::from_validated_descriptors(
        local,
        test_dht_protocol(),
        NOW_MS,
        &OnionServiceName::tcp(),
        online_nodes,
        exits,
    )
}

/// Replay a fixed draw sequence, then `0`: every draw past the sequence takes the first
/// candidate by DID order.
struct FixedEntropy {
    values: VecDeque<u64>,
}

impl FixedEntropy {
    /// Replay `values` in order.
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

/// Assert the loop laws of a selected route: `H = 5` positions, the guard at `1` and `H` only,
/// the exit at the symbol position, `H − 1` distinct hops, and the Phase 1 view `g, r₀,₂, h₁`.
fn assert_session_loop(route: &OnionRoute) {
    let hops = route.hops();
    let dids = hops.positions().map(|hop| hop.did).collect::<Vec<_>>();
    let guard = hops.guard().did;

    assert_eq!(dids.len(), 5);
    assert_eq!(dids.first(), Some(&guard));
    assert_eq!(dids.last(), Some(&guard));
    assert_eq!(dids.iter().filter(|did| **did == guard).count(), 2);
    assert_eq!(dids.get(2), Some(&route.exit_did()));
    assert_eq!(
        hops.open_path()
            .map(|hop| hop.did)
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
    let prefix = route.forward_path();
    assert_eq!(
        prefix
            .relays()
            .iter()
            .map(|hop| hop.did)
            .collect::<Vec<_>>(),
        dids.get(..2).map(<[Did]>::to_vec).unwrap_or_default()
    );
    assert_eq!(prefix.terminal().did, route.exit_did());
}

/// Selection places the `tcp` registrant at the symbol position and relay registrants
/// everywhere else, closing the loop through one guard (D4c, D5, L7).
#[test]
fn test_route_is_a_guard_closed_loop_of_registrants() -> Result<()> {
    let local = node_key(1)?.delegator_did();
    let relays = node_keys(2..=4)?;
    let exit = node_key(5)?;
    let online = relays
        .iter()
        .chain([&exit])
        .map(live_relay)
        .collect::<Result<Vec<_>>>()?;
    let registrants = relays
        .iter()
        .chain([&exit])
        .map(|key| key.delegator_did())
        .collect::<BTreeSet<_>>();

    let route = select_onion_route_from_candidates(
        &tcp_request(),
        tcp_candidates(local, online, [live_exit(&exit)?]),
        Vec::new(),
        &mut FixedEntropy::new([]),
        |_| true,
    )?;

    assert_session_loop(&route);
    assert_eq!(route.exit_did(), exit.delegator_did());
    assert_eq!(route.service(), "tcp");
    assert!(route
        .hops()
        .positions()
        .all(|hop| registrants.contains(&hop.did) && hop.process_epoch == TEST_PROCESS_EPOCH));
    assert!(route.hops().positions().all(|hop| hop.did != local));
    Ok(())
}

/// Fail closed (D5): below `H − 1 = 4` distinct relay registrants selection fails and never
/// shortens the loop; a node without the relay capability is not counted.
#[test]
fn test_selection_fails_closed_below_distinct_hop_bound() -> Result<()> {
    let local = node_key(1)?.delegator_did();
    let relays = node_keys(2..=3)?;
    let bystander = node_key(4)?;
    let exit = node_key(5)?;
    let mut online = relays
        .iter()
        .chain([&exit])
        .map(live_relay)
        .collect::<Result<Vec<_>>>()?;
    online.push(online_descriptor(
        &bystander,
        OnlineNodeCapabilities::default(),
        TEST_NETWORK_ID,
        EXPIRES_MS,
    )?);

    let result = select_onion_route_from_candidates(
        &tcp_request(),
        tcp_candidates(local, online, [live_exit(&exit)?]),
        Vec::new(),
        &mut FixedEntropy::new([]),
        |_| true,
    );

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NotEnoughLoopHops {
            required: 4,
            eligible: 3,
        }))
    ));
    Ok(())
}

/// Registration (D2): an exit is selected only while its process registers `relay` at the same
/// epoch. Each rejection names its cause: no relay registration at all, or a relay registration
/// of another process (stale).
#[test]
fn test_exit_requires_its_current_relay_registration() -> Result<()> {
    let local = node_key(1)?.delegator_did();
    let relays = node_keys(2..=4)?;
    let exit = node_key(5)?;
    let relay_less = node_key(6)?;
    let online = relays
        .iter()
        .chain([&exit])
        .map(live_relay)
        .chain([online_descriptor(
            &relay_less,
            OnlineNodeCapabilities::default(),
            TEST_NETWORK_ID,
            EXPIRES_MS,
        )])
        .collect::<Result<Vec<_>>>()?;
    let stale = exit_descriptor(&exit, STALE_PROCESS_EPOCH, TEST_NETWORK_ID, EXPIRES_MS)?;
    let select = |exits: Vec<OnionExitDescriptor>| {
        select_onion_route_from_candidates(
            &tcp_request(),
            tcp_candidates(local, online.clone(), exits),
            Vec::new(),
            &mut FixedEntropy::new([]),
            |_| true,
        )
    };

    assert!(matches!(
        select(vec![stale]),
        Err(Error::OnionRouteError(OnionRouteError::ExitRelayRegistrationMismatch { service }))
            if service == "tcp"
    ));
    assert!(matches!(
        select(vec![live_exit(&relay_less)?, live_exit(&node_key(7)?)?]),
        Err(Error::OnionRouteError(OnionRouteError::ExitWithoutRelayRegistration { service }))
            if service == "tcp"
    ));
    assert_eq!(
        select(vec![live_exit(&relay_less)?, live_exit(&exit)?])?.exit_did(),
        exit.delegator_did()
    );
    Ok(())
}

/// A route cannot be built over a symbol hop whose epoch differs from its descriptor's.
#[test]
fn test_route_rejects_symbol_hop_with_stale_epoch() -> Result<()> {
    let keys = node_keys(1..=4)?;
    let exit = keys.get(2).ok_or(Error::InvalidData)?;
    let mut next = keys.iter().map(|key| hop(key, STALE_PROCESS_EPOCH));
    let stale = OnionLoop::try_unfold(
        OnionPipelineSymbols::new(&[], &()),
        &mut next,
        |next, _| next.next().ok_or(Error::InvalidData),
        |next, _| next.next().ok_or(Error::InvalidData),
    )?;

    assert!(matches!(
        OnionRoute::new(OnionServiceName::tcp(), stale, live_exit(exit)?),
        Err(Error::OnionRouteError(OnionRouteError::ExitHopMismatch))
    ));
    Ok(())
}

/// Descriptors past their expiry are not candidates.
#[test]
fn test_directory_candidates_reject_expired_remote_descriptors() -> Result<()> {
    let local = node_key(1)?.delegator_did();
    let relay = node_key(2)?;
    let expired_at = 40;

    let candidates = tcp_candidates(
        local,
        [online_descriptor(
            &relay,
            OnlineNodeCapabilities::onion_relay(TEST_PROCESS_EPOCH),
            TEST_NETWORK_ID,
            expired_at,
        )?],
        [exit_descriptor(
            &relay,
            TEST_PROCESS_EPOCH,
            TEST_NETWORK_ID,
            expired_at,
        )?],
    );

    assert!(candidates.relays.is_empty());
    assert!(candidates.exits.is_empty());
    Ok(())
}

/// Descriptors signed for another network are not candidates.
#[test]
fn test_directory_candidates_reject_foreign_network_descriptors() -> Result<()> {
    let local = node_key(1)?.delegator_did();
    let relay = node_key(2)?;
    let foreign_network = 2;

    let candidates = tcp_candidates(
        local,
        [online_descriptor(
            &relay,
            OnlineNodeCapabilities::onion_relay(TEST_PROCESS_EPOCH),
            foreign_network,
            EXPIRES_MS,
        )?],
        [exit_descriptor(
            &relay,
            TEST_PROCESS_EPOCH,
            foreign_network,
            EXPIRES_MS,
        )?],
    );

    assert!(candidates.relays.is_empty());
    assert!(candidates.exits.is_empty());
    Ok(())
}

/// Without a symbol registrant the route reports the service before any guard policy runs.
#[test]
fn test_route_builder_reports_no_live_exit_before_guard_filter() {
    let candidates = OnionRouteCandidates {
        relays: Vec::new(),
        exits: Vec::new(),
    };

    let result = select_onion_route_from_candidates(
        &tcp_request(),
        candidates,
        Vec::new(),
        &mut FixedEntropy::new([]),
        |_| false,
    );

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service
        })) if service == "tcp"
    ));
}

/// The candidates of `relays` plus the registrant `exit`, both at the current epoch.
fn candidates_with_exit(
    relays: &[DelegateeKey],
    exit: &DelegateeKey,
) -> Result<OnionRouteCandidates> {
    Ok(OnionRouteCandidates {
        relays: relays
            .iter()
            .chain([exit])
            .map(|key| hop(key, TEST_PROCESS_EPOCH))
            .collect(),
        exits: vec![live_exit(exit)?],
    })
}

/// Draw a loop for the pipeline whose symbols are registered by the hops of `symbols`, and
/// relabel its symbol positions by those hops.
fn draw_loop(
    symbols: &[Vec<OnionRouteHop>],
    relays: &[OnionRouteHop],
    entropy: &mut FixedEntropy,
) -> Result<OnionLoop<OnionRouteHop>> {
    let relays = RelayRegistrants::new(relays.to_vec());
    let admitted = symbols
        .iter()
        .map(|registrants| relays.admit(registrants.iter().copied(), |hop| *hop))
        .collect::<Vec<_>>();
    let (terminal, intermediate) = admitted.split_last().ok_or(Error::InvalidData)?;
    select_loop(
        OnionPipelineSymbols::new(intermediate, terminal),
        &relays,
        &BTreeMap::new(),
        entropy,
        |_| true,
    )
    .map(|drawn| drawn.project_symbols(|(hop, _)| *hop).0)
}

/// The guard is drawn by quality weight, over the relays in DID order: after the exit's draw, a
/// healthy guard (weight 8) wins a draw of `1` against a degraded one (weight 1) before it.
#[test]
fn test_guard_is_drawn_by_quality_weight() -> Result<()> {
    let relays = node_keys(1..=3)?;
    let exit = node_key(4)?;
    let (degraded, healthy) = match in_did_order(relays.as_slice()).as_slice() {
        [first, second, ..] => (first.delegator_did(), second.delegator_did()),
        _ => return Err(Error::InvalidData),
    };

    let route = select_onion_route_from_candidates(
        &tcp_request(),
        candidates_with_exit(relays.as_slice(), &exit)?,
        vec![
            (degraded, PeerQuality::Degraded),
            (healthy, PeerQuality::Healthy),
        ],
        &mut FixedEntropy::new([0, 1]),
        |did| did == degraded || did == healthy,
    )?;

    assert_session_loop(&route);
    assert_eq!(route.hops().guard().did, healthy);
    Ok(())
}

/// The guard is drawn from the permitted first hops only; later relays need no permission.
#[test]
fn test_guard_policy_constrains_the_guard_only() -> Result<()> {
    let relays = node_keys(1..=3)?;
    let exit = node_key(4)?;
    let direct = relays
        .last()
        .map(DelegateeKey::delegator_did)
        .ok_or(Error::InvalidData)?;

    let route = select_onion_route_from_candidates(
        &tcp_request(),
        candidates_with_exit(relays.as_slice(), &exit)?,
        Vec::new(),
        &mut FixedEntropy::new([]),
        |did| did == direct,
    )?;

    assert_session_loop(&route);
    assert_eq!(route.hops().guard().did, direct);
    Ok(())
}

/// Hall's filter keeps the sole symbol registrant out of the guard: it is first in DID order and
/// a permitted guard, so an unfiltered draw of `0` would take it, and the symbol position would
/// be stranded.
#[test]
fn test_hall_filter_keeps_the_sole_registrant_out_of_the_guard() -> Result<()> {
    let keys = node_keys(1..=4)?;
    let ordered = in_did_order(keys.as_slice());
    let registrant = ordered
        .first()
        .map(|key| key.delegator_did())
        .ok_or(Error::InvalidData)?;
    let relays = ordered
        .iter()
        .map(|key| hop(key, TEST_PROCESS_EPOCH))
        .collect::<Vec<_>>();

    let registrant_hop = relays
        .iter()
        .copied()
        .find(|hop| hop.did == registrant)
        .ok_or(Error::InvalidData)?;
    let hops = draw_loop(
        &[vec![registrant_hop]],
        relays.as_slice(),
        &mut FixedEntropy::new([]),
    )?;

    assert_ne!(hops.guard().did, registrant);
    assert_eq!(hops.symbol(1).map(|hop| hop.did), Some(registrant));
    Ok(())
}

/// A permitted guard that is also a registrant of the symbol is still drawn as guard when
/// another registrant can take the symbol position.
#[test]
fn test_guard_never_strands_the_symbol_position() -> Result<()> {
    let relays = node_keys(1..=2)?;
    let direct = node_key(3)?;
    let remote = node_key(4)?;
    let direct_did = direct.delegator_did();
    let candidates = OnionRouteCandidates {
        relays: relays
            .iter()
            .chain([&direct, &remote])
            .map(|key| hop(key, TEST_PROCESS_EPOCH))
            .collect(),
        exits: vec![live_exit(&direct)?, live_exit(&remote)?],
    };

    let route = select_onion_route_from_candidates(
        &tcp_request(),
        candidates,
        Vec::new(),
        &mut FixedEntropy::new([]),
        |did| did == direct_did,
    )?;

    assert_session_loop(&route);
    assert_eq!(route.hops().guard().did, direct_did);
    assert_eq!(route.exit_did(), remote.delegator_did());
    Ok(())
}

/// The only permitted guard cannot be the only symbol registrant: no loop exists.
#[test]
fn test_route_rejects_loop_without_permitted_guard() -> Result<()> {
    let relays = node_keys(1..=3)?;
    let exit = node_key(4)?;
    let exit_did = exit.delegator_did();
    let outsider = node_key(5)?.delegator_did();

    for permitted in [exit_did, outsider] {
        let result = select_onion_route_from_candidates(
            &tcp_request(),
            candidates_with_exit(relays.as_slice(), &exit)?,
            Vec::new(),
            &mut FixedEntropy::new([]),
            |did| did == permitted,
        );

        assert!(matches!(
            result,
            Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))
        ));
    }
    Ok(())
}

/// Loop shape over `n` symbols: `H = 3n + 2`, the guard at `1` and `H` only, the `k`-th symbol
/// hop at `3k` drawn from the registrants of symbol `k`, every other position a relay registrant,
/// and `H − 1` distinct hops.
#[test]
fn test_select_loop_places_each_symbol_on_its_registrants() -> Result<()> {
    for symbols in 1..=MAX_ONION_LOOP_SYMBOLS {
        let shape = OnionLoopShape::new(symbols)?;
        let count = u8::try_from(shape.distinct_hops()).map_err(|_| Error::InvalidData)?;
        let keys = node_keys(1..=count)?;
        let relays = keys
            .iter()
            .map(|key| hop(key, TEST_PROCESS_EPOCH))
            .collect::<Vec<_>>();
        let registrants = relays
            .chunks(2)
            .take(symbols)
            .map(<[OnionRouteHop]>::to_vec)
            .collect::<Vec<_>>();

        let hops = draw_loop(
            registrants.as_slice(),
            relays.as_slice(),
            &mut FixedEntropy::new([]),
        )?;
        let dids = hops.positions().map(|hop| hop.did).collect::<Vec<_>>();

        assert_eq!(dids.len(), 3 * symbols + 2);
        assert_eq!(dids.first(), dids.last());
        assert!(!has_duplicate_dids(&hops));
        for (k, registrant) in registrants.iter().enumerate() {
            let symbol_hop = hops.symbol(k + 1);
            assert!(symbol_hop.is_some_and(|hop| registrant.contains(hop)));
            assert_eq!(
                symbol_hop.map(|hop| hop.did).as_ref(),
                dids.get(3 * (k + 1) - 1)
            );
        }
    }
    Ok(())
}

/// `n > n_max` is rejected before any draw.
#[test]
fn test_select_loop_rejects_more_than_max_symbols() -> Result<()> {
    let keys = node_keys(1..=16)?;
    let relays = keys
        .iter()
        .map(|key| hop(key, TEST_PROCESS_EPOCH))
        .collect::<Vec<_>>();

    assert!(matches!(
        draw_loop(
            &vec![relays.clone(); MAX_ONION_LOOP_SYMBOLS + 1],
            relays.as_slice(),
            &mut FixedEntropy::new([]),
        ),
        Err(Error::OnionRouteError(
            OnionRouteError::LoopSymbolsOutOfBounds {
                symbols: 5,
                max_symbols: 4,
            }
        ))
    ));
    Ok(())
}

/// Hall's condition steers the symbol draws: with `S₁ = {a}` and `S₂ = {a, b}` every loop puts
/// `a` at `h₁` and `b` at `h₂`, and with `S₁ = S₂ = {a}` no loop exists.
#[test]
fn test_select_loop_assigns_distinct_symbol_hops_when_possible() -> Result<()> {
    let keys = node_keys(1..=7)?;
    let relays = keys
        .iter()
        .map(|key| hop(key, TEST_PROCESS_EPOCH))
        .collect::<Vec<_>>();
    let (a, b) = match relays.as_slice() {
        [first, second, ..] => (*first, *second),
        _ => return Err(Error::InvalidData),
    };

    for draw in [0, 1, u64::MAX] {
        let hops = draw_loop(
            &[vec![a], vec![a, b]],
            relays.as_slice(),
            &mut FixedEntropy::new([draw; 8]),
        )?;

        assert_eq!(hops.symbol(1), Some(&a));
        assert_eq!(hops.symbol(2), Some(&b));
        assert!(!has_duplicate_dids(&hops));
    }
    assert!(matches!(
        draw_loop(
            &[vec![a], vec![a]],
            relays.as_slice(),
            &mut FixedEntropy::new([]),
        ),
        Err(Error::OnionRouteError(
            OnionRouteError::NoDistinctSymbolHops
        ))
    ));
    Ok(())
}

/// Hall's condition counts only drawable candidates: a registrant outside the relays cannot
/// vouch for the symbol position. With `S = {x, a}`, `x` registering no relay, the guard draw
/// must leave `a` to the symbol although `a` is first in DID order and a permitted guard.
#[test]
fn test_hall_filter_ignores_registrants_outside_the_relays() -> Result<()> {
    let keys = node_keys(1..=5)?;
    let ordered = in_did_order(keys.as_slice());
    let (phantom, relays) = match ordered.split_last() {
        Some((phantom, relays)) => (
            hop(phantom, TEST_PROCESS_EPOCH),
            relays
                .iter()
                .map(|key| hop(key, TEST_PROCESS_EPOCH))
                .collect::<Vec<_>>(),
        ),
        None => return Err(Error::InvalidData),
    };
    let first = *relays.first().ok_or(Error::InvalidData)?;

    let hops = draw_loop(
        &[vec![phantom, first]],
        relays.as_slice(),
        &mut FixedEntropy::new([]),
    )?;

    assert_ne!(hops.guard().did, first.did);
    assert_eq!(hops.symbol(1), Some(&first));
    assert!(hops.positions().all(|hop| hop.did != phantom.did));
    Ok(())
}

/// Guard closure (L7): `has_duplicate_dids` admits the guard exactly twice, at `1` and `H`, and
/// rejects any other repetition, of the guard or of another hop.
#[test]
fn test_has_duplicate_dids_admits_exactly_the_guard_twice() -> Result<()> {
    let keys = node_keys(1..=4)?;
    let labelled = |order: [usize; 4]| {
        let mut next = order
            .into_iter()
            .filter_map(|index| keys.get(index))
            .map(|key| hop(key, TEST_PROCESS_EPOCH));
        OnionLoop::try_unfold(
            OnionPipelineSymbols::new(&[], &()),
            &mut next,
            |next, _| next.next().ok_or(Error::InvalidData),
            |next, _| next.next().ok_or(Error::InvalidData),
        )
    };

    let distinct = labelled([0, 1, 2, 3])?;
    assert!(!has_duplicate_dids(&distinct));
    assert_eq!(
        distinct
            .positions()
            .filter(|hop| hop.did == distinct.guard().did)
            .count(),
        2
    );
    assert!(has_duplicate_dids(&labelled([0, 0, 2, 3])?));
    assert!(has_duplicate_dids(&labelled([0, 1, 2, 0])?));
    assert!(has_duplicate_dids(&labelled([0, 1, 2, 1])?));
    Ok(())
}

/// A route admits only the loop of its one-symbol pipeline.
#[test]
fn test_route_rejects_loop_of_another_pipeline_shape() -> Result<()> {
    let keys = node_keys(1..=7)?;
    let exit = keys.get(2).ok_or(Error::InvalidData)?;
    let mut next = keys.iter().map(|key| hop(key, TEST_PROCESS_EPOCH));
    let hops = OnionLoop::try_unfold(
        OnionPipelineSymbols::new(&[()], &()),
        &mut next,
        |next, _| next.next().ok_or(Error::InvalidData),
        |next, _| next.next().ok_or(Error::InvalidData),
    )?;

    assert!(matches!(
        OnionRoute::new(OnionServiceName::tcp(), hops, live_exit(exit)?),
        Err(Error::OnionRouteError(OnionRouteError::LoopShapeMismatch {
            expected: 1,
            actual: 2,
        }))
    ));
    Ok(())
}

/// Draw order (L7): the exit is drawn before the guard and the relays, so its marginal is its
/// quality share among the exits, and a high-quality exit is not consumed as the guard first.
///
/// With `R = {x, a, b, c}`, exits `{x (Healthy, 8), a (Unknown, 4)}` and every relay a permitted
/// guard, the exit roll is taken modulo `12` and the guard roll modulo `12` or `16`, so every
/// pair of rolls in `[0, 48)²` enumerates the two draws exactly:
///
/// ```text
/// P(exit = x)  = 8/12            = 2/3     1536 of 2304
/// P(guard = x) = P(exit = a)·8/16 = 1/6     384 of 2304
/// ```
#[test]
fn test_exit_and_guard_marginals_follow_the_draw_order() -> Result<()> {
    let keys = node_keys(1..=4)?;
    let (x, a) = match keys.as_slice() {
        [x, a, ..] => (x.delegator_did(), a.delegator_did()),
        _ => return Err(Error::InvalidData),
    };
    let candidates = OnionRouteCandidates {
        relays: keys
            .iter()
            .map(|key| hop(key, TEST_PROCESS_EPOCH))
            .collect(),
        exits: keys
            .get(..2)
            .unwrap_or_default()
            .iter()
            .map(live_exit)
            .collect::<Result<Vec<_>>>()?,
    };
    let mut exits_at_x = 0;
    let mut guards_at_x = 0;

    for exit_roll in 0..48 {
        for guard_roll in 0..48 {
            let route = select_onion_route_from_candidates(
                &tcp_request(),
                candidates.clone(),
                vec![(x, PeerQuality::Healthy)],
                &mut FixedEntropy::new([exit_roll, guard_roll]),
                |_| true,
            )?;
            exits_at_x += usize::from(route.exit_did() == x);
            guards_at_x += usize::from(route.hops().guard().did == x);
            assert!([x, a].contains(&route.exit_did()));
        }
    }

    assert_eq!(exits_at_x, 1536);
    assert_eq!(guards_at_x, 384);
    Ok(())
}

/// Fail closed first (D5): below `H − 1` relays the error is `NotEnoughLoopHops`, whatever the
/// guard policy admits.
#[test]
fn test_fail_closed_is_checked_before_the_guard_policy() -> Result<()> {
    let relays = node_keys(1..=2)?;
    let exit = node_key(3)?;

    let result = select_onion_route_from_candidates(
        &tcp_request(),
        candidates_with_exit(relays.as_slice(), &exit)?,
        Vec::new(),
        &mut FixedEntropy::new([]),
        |_| false,
    );

    assert!(matches!(
        result,
        Err(Error::OnionRouteError(OnionRouteError::NotEnoughLoopHops {
            required: 4,
            eligible: 3,
        }))
    ));
    Ok(())
}
