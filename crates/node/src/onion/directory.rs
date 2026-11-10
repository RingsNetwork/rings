//! Directory reader boundary for onion route construction.

use std::collections::BTreeSet;

use rings_core::dht::Did;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;
use rings_core::utils::Clock;
use rings_core::utils::SharedRng;
use rings_core::utils::SystemClock;

use super::invoke_descriptor::OnionInvokeDescriptor;
use super::pipeline::OnionInvokePipeline;
use super::route::select_onion_pipeline_route_from_candidates;
use super::route::OnionPipelineRoute;
use super::route::OnionPipelineRouteCandidates;
use super::route::OnionPipelineRouteRequest;
use super::select_onion_route_from_candidates;
use super::OnionEntryGuards;
use super::OnionExitDescriptor;
use super::OnionRoute;
use super::OnionRouteCandidates;
use super::OnionRouteError;
use super::OnionRouteRequest;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::online::OnlineNodeDescriptor;

/// Read-only directory effects required by onion route construction.
#[cfg_attr(rings_browser, async_trait::async_trait(?Send))]
#[cfg_attr(rings_native, async_trait::async_trait)]
pub(crate) trait OnionDirectoryReader: rings_runtime::MaybeSendSync {
    /// Return the local DID that must not appear as a selected relay.
    fn local_did(&self) -> Did;

    /// Return the local DHT protocol mode used to reject foreign directory entries.
    fn dht_protocol_mode(&self) -> DhtProtocolMode;

    /// Return live online-node descriptors eligible for relay filtering.
    async fn live_online_nodes(&self) -> Result<Vec<OnlineNodeDescriptor>>;

    /// Return live onion exits that offer `service`.
    async fn live_onion_exits(&self, service: &str) -> Result<Vec<OnionExitDescriptor>>;

    /// Return live intermediate `invoke` descriptors across the directory.
    async fn live_onion_invokes(&self) -> Result<Vec<OnionInvokeDescriptor>> {
        Ok(Vec::new())
    }

    /// Return local peer-quality observations for route weighting.
    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)>;

    /// Return the DIDs of directly connected peers eligible as first-hop guards.
    fn direct_peer_dids(&self) -> BTreeSet<Did>;

    /// Return the local entry-guard manager for this client.
    fn onion_entry_guards(&self) -> &OnionEntryGuards;

    /// Return the current Unix epoch timestamp in milliseconds for directory validation.
    fn now_ms(&self) -> u128 {
        SystemClock.now_ms()
    }

    /// Return the cryptographic RNG capability used to sample entry guards and route hops.
    fn route_rng(&self) -> SharedRng {
        SharedRng::system()
    }
}

/// Build an onion proxy route for a concrete target whose first hop is a directly connected peer.
pub(crate) async fn build_onion_proxy_route(
    reader: &impl OnionDirectoryReader,
    proxy: OnionProxyProtocol,
    target: OnionProxyTarget,
) -> Result<OnionProxyRoute> {
    let direct_peers = reader.direct_peer_dids();
    let allowed_peers = direct_peers.clone();
    let route = build_onion_proxy_route_with_first_hop(reader, proxy, target, move |did| {
        allowed_peers.contains(&did)
    })
    .await?;
    let first_hop = route.route.hops().guard().did;
    if first_hop != reader.local_did() && direct_peers.contains(&first_hop) {
        return Ok(route);
    }
    Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))
}

pub(crate) async fn build_onion_proxy_route_with_first_hop(
    reader: &impl OnionDirectoryReader,
    proxy: OnionProxyProtocol,
    target: OnionProxyTarget,
    first_hop_permitted: impl Fn(Did) -> bool,
) -> Result<OnionProxyRoute> {
    let service_name = proxy.exit_service_name();
    let service = service_name.as_str().to_string();
    let now_ms = reader.now_ms();
    let directory_exits = OnionExitDescriptor::latest_valid_by_service_did(
        reader.live_onion_exits("").await?,
        now_ms,
        reader.dht_protocol_mode().network_id,
        false,
    );
    let service_exits = directory_exits
        .into_iter()
        .filter(|exit| exit.service == service_name)
        .collect::<Vec<_>>();
    if service_exits.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service,
        }));
    }
    let protocol_exits = service_exits
        .into_iter()
        .filter(|exit| proxy.accepts_exit_descriptor(exit))
        .collect::<Vec<_>>();
    if protocol_exits.is_empty() {
        return Err(Error::OnionRouteError(
            OnionRouteError::NoExitForProxyProtocol {
                service: service.clone(),
                protocol: proxy.label().to_string(),
            },
        ));
    }
    let policy_exits = protocol_exits
        .into_iter()
        .filter(|exit| exit.policy.allows_target(&target))
        .collect::<Vec<_>>();
    if policy_exits.is_empty() {
        return Err(Error::OnionRouteError(
            OnionRouteError::NoExitAllowsTarget {
                service,
                target: target.authority(),
            },
        ));
    }
    let request = OnionRouteRequest::from_service_name(service_name);
    let route =
        build_onion_route_from_exits(reader, request, policy_exits, first_hop_permitted).await?;

    Ok(OnionProxyRoute { target, route })
}

/// Build a guard-closed multi-hop route for an intermediate `invoke` Kleisli pipeline
/// whose first hop is a directly connected peer.
pub(crate) async fn build_onion_pipeline_route(
    reader: &impl OnionDirectoryReader,
    pipeline: &OnionInvokePipeline,
) -> Result<OnionPipelineRoute> {
    let direct_peers = reader.direct_peer_dids();
    let allowed_peers = direct_peers.clone();
    let route = build_onion_pipeline_route_with_first_hop(reader, pipeline, move |did| {
        allowed_peers.contains(&did)
    })
    .await?;
    let first_hop = route.hops().guard().did;
    if first_hop != reader.local_did() && direct_peers.contains(&first_hop) {
        return Ok(route);
    }
    Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))
}

pub(crate) async fn build_onion_pipeline_route_with_first_hop(
    reader: &impl OnionDirectoryReader,
    pipeline: &OnionInvokePipeline,
    first_hop_permitted: impl Fn(Did) -> bool,
) -> Result<OnionPipelineRoute> {
    let request = OnionPipelineRouteRequest::from_pipeline(pipeline);
    let online_nodes = reader.live_online_nodes().await?;
    let invokes = reader.live_onion_invokes().await?;
    let now_ms = reader.now_ms();
    let candidates = OnionPipelineRouteCandidates::from_validated_descriptors(
        reader.local_did(),
        reader.dht_protocol_mode(),
        now_ms,
        online_nodes,
        invokes,
    );
    let qualities = reader.peer_qualities().await;
    let mut entropy = reader.route_rng();
    let first_hop_permitted = &first_hop_permitted;
    let guard_set = reader
        .onion_entry_guards()
        .select_relay_guards_at(
            reader.dht_protocol_mode().network_id,
            &candidates.relays,
            &qualities,
            now_ms,
            &mut entropy,
            first_hop_permitted,
        )
        .await?;
    select_onion_pipeline_route_from_candidates(
        &request,
        candidates,
        qualities,
        &mut entropy,
        |did| guard_set.contains(did) && first_hop_permitted(did),
    )
}

async fn build_onion_route_from_exits(
    reader: &impl OnionDirectoryReader,
    request: OnionRouteRequest,
    exits: Vec<OnionExitDescriptor>,
    first_hop_permitted: impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let online_nodes = reader.live_online_nodes().await?;
    let now_ms = reader.now_ms();
    let candidates = OnionRouteCandidates::from_validated_descriptors(
        reader.local_did(),
        reader.dht_protocol_mode(),
        now_ms,
        request.service_name(),
        online_nodes,
        exits,
    );
    let qualities = reader.peer_qualities().await;
    let mut entropy = reader.route_rng();
    let first_hop_permitted = &first_hop_permitted;
    let guard_set = reader
        .onion_entry_guards()
        .select_relay_guards_at(
            reader.dht_protocol_mode().network_id,
            &candidates.relays,
            &qualities,
            now_ms,
            &mut entropy,
            first_hop_permitted,
        )
        .await?;
    select_onion_route_from_candidates(&request, candidates, qualities, &mut entropy, |did| {
        guard_set.contains(did) && first_hop_permitted(did)
    })
}
