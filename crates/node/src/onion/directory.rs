//! Directory reader boundary for onion route construction.

use rings_core::dht::Did;
use rings_core::measure::PeerQuality;

use super::select_onion_route_from_candidates;
use super::OnionExitDescriptor;
use super::OnionExitTarget;
use super::OnionRoute;
use super::OnionRouteCandidates;
use super::OnionRouteError;
use super::OnionRouteRequest;
use super::SystemRouteEntropy;
use crate::error::Error;
use crate::error::Result;
use crate::onion::proxy::OnionProxyConfig;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;
use crate::online::OnlineNodeDescriptor;
use crate::rings_name::RingsNameRecord;

/// Read-only directory effects required by onion route construction.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub(crate) trait OnionDirectoryReader {
    /// Return the local DID that must not appear as a selected relay.
    fn local_did(&self) -> Did;

    /// Return live online-node descriptors eligible for relay filtering.
    async fn live_online_nodes(&self) -> Result<Vec<OnlineNodeDescriptor>>;

    /// Return live onion exits that offer `service`.
    async fn live_onion_exits(&self, service: &str) -> Result<Vec<OnionExitDescriptor>>;

    /// Return local peer-quality observations for route weighting.
    async fn peer_qualities(&self) -> Vec<(Did, PeerQuality)>;
}

/// Build an onion route from live directory descriptors.
pub(crate) async fn build_onion_route(
    reader: &impl OnionDirectoryReader,
    service: String,
    hop_count: usize,
    allow_short_paths: bool,
) -> Result<OnionRoute> {
    let request = OnionRouteRequest::new(service, hop_count, allow_short_paths)?;
    build_filtered_onion_route(reader, request, |_| true).await
}

/// Build an onion proxy route for a concrete target.
pub(crate) async fn build_onion_proxy_route(
    reader: &impl OnionDirectoryReader,
    proxy: OnionProxyConfig,
    target: OnionProxyTarget,
) -> Result<OnionProxyRoute> {
    let service_name = proxy.exit_service_name().clone();
    let service = service_name.as_str().to_string();
    let transport = proxy.exit_transport();
    let exit_target = OnionExitTarget::from_proxy_target(&target);
    let service_exits = reader
        .live_onion_exits("")
        .await?
        .into_iter()
        .filter(|exit| exit.advertises_service_name(service_name.as_str()))
        .collect::<Vec<_>>();
    if service_exits.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service,
        }));
    }
    let transport_exits = service_exits
        .into_iter()
        .filter(|exit| exit.offers_service_transport(service_name.as_str(), transport))
        .collect::<Vec<_>>();
    if transport_exits.is_empty() {
        return Err(Error::OnionRouteError(
            OnionRouteError::NoExitWithTransport { service, transport },
        ));
    }
    let policy_exits = transport_exits
        .into_iter()
        .filter(|exit| exit.policy.allows_target(&exit_target))
        .collect::<Vec<_>>();
    if policy_exits.is_empty() {
        return Err(Error::OnionRouteError(
            OnionRouteError::NoExitAllowsTarget {
                service,
                target: exit_target.authority().to_string(),
            },
        ));
    }
    let request = OnionRouteRequest::from_service_name(
        service_name,
        proxy.hop_count,
        proxy.allow_short_paths,
    );
    let route = build_onion_route_from_exits(reader, request, policy_exits).await?;

    Ok(OnionProxyRoute {
        protocol: proxy.protocol,
        target,
        route,
    })
}

/// Build an onion route to the live target descriptor authenticated by a `.rings` record.
pub(crate) async fn build_rings_name_route(
    reader: &impl OnionDirectoryReader,
    record: RingsNameRecord,
    hop_count: usize,
    allow_short_paths: bool,
) -> Result<OnionRoute> {
    let request =
        OnionRouteRequest::from_service_name(record.service.clone(), hop_count, allow_short_paths);
    let exits = reader
        .live_onion_exits(record.service.as_str())
        .await?
        .into_iter()
        .filter(|exit| exit.did == record.target_did)
        .filter(|exit| exit.session_public_key == record.session_public_key)
        .filter(|exit| exit.matches_network(record.network_id))
        .filter(|exit| exit.offers_service_transport(record.service.as_str(), record.transport))
        .collect::<Vec<_>>();
    if exits.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoRingsNameTarget {
            name: record.name.to_string(),
            target: record.target_did,
            service: record.service.into(),
            transport: record.transport,
        }));
    }

    build_onion_route_from_exits(reader, request, exits).await
}

async fn build_filtered_onion_route(
    reader: &impl OnionDirectoryReader,
    request: OnionRouteRequest,
    exit_filter: impl Fn(&OnionExitDescriptor) -> bool,
) -> Result<OnionRoute> {
    let exits = reader
        .live_onion_exits(request.service())
        .await?
        .into_iter()
        .filter(exit_filter)
        .collect::<Vec<_>>();
    build_onion_route_from_exits(reader, request, exits).await
}

async fn build_onion_route_from_exits(
    reader: &impl OnionDirectoryReader,
    request: OnionRouteRequest,
    exits: Vec<OnionExitDescriptor>,
) -> Result<OnionRoute> {
    let online_nodes = reader.live_online_nodes().await?;
    let candidates = OnionRouteCandidates::from_validated_descriptors(
        reader.local_did(),
        request.service_name(),
        online_nodes,
        exits,
    );
    select_onion_route_from_candidates(
        &request,
        candidates,
        reader.peer_qualities().await,
        &mut SystemRouteEntropy::new(),
    )
}
