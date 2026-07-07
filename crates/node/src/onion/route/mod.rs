//! Onion route selection.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::circuit::MAX_ONION_CIRCUIT_HOPS;
use super::OnionExitDescriptor;
use super::OnionRouteError;
use super::OnionServiceName;
use super::ONION_RELAY_CAPABILITY;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;

/// Default number of DID hops in a production onion route, including the exit.
pub const DEFAULT_ONION_ROUTE_HOPS: usize = 3;

/// Route-building request for an onion circuit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRouteRequest {
    /// Exit service required by the route.
    pub service: OnionServiceName,
    /// Desired hop count including the exit. `0` uses [`DEFAULT_ONION_ROUTE_HOPS`].
    pub hop_count: usize,
    /// Whether a route may be shorter than `hop_count` when the network is too small.
    pub allow_short_paths: bool,
}

impl OnionRouteRequest {
    /// Build a route request from an untrusted service string.
    pub fn new(
        service: impl AsRef<str>,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Result<Self> {
        Ok(Self::from_service_name(
            parse_route_service(service)?,
            hop_count,
            allow_short_paths,
        ))
    }

    /// Build a route request from an already canonical service name.
    pub fn from_service_name(
        service: OnionServiceName,
        hop_count: usize,
        allow_short_paths: bool,
    ) -> Self {
        Self {
            service,
            hop_count,
            allow_short_paths,
        }
    }

    /// Return the canonical service selected by this request.
    pub fn service(&self) -> &str {
        self.service.as_str()
    }

    pub(crate) fn service_name(&self) -> &OnionServiceName {
        &self.service
    }

    fn target_hop_count(&self) -> usize {
        if self.hop_count == 0 {
            DEFAULT_ONION_ROUTE_HOPS
        } else {
            self.hop_count
        }
    }
}

/// One hop selected for encrypted onion routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionRouteHop {
    /// Hop DID.
    pub did: Did,
    /// Hop session public key used for ElGamal-AEAD layers.
    pub session_public_key: PublicKey<33>,
}

impl OnionRouteHop {
    /// Build a route hop from its DID and session public key.
    pub const fn new(did: Did, session_public_key: PublicKey<33>) -> Self {
        Self {
            did,
            session_public_key,
        }
    }
}

/// Selected onion route.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRoute {
    /// Exit service requested by the route.
    service: OnionServiceName,
    /// Ordered DIDs, ending with the exit DID.
    hops: Vec<Did>,
    /// Ordered encrypted route hops, ending with the exit hop.
    encryption_hops: Vec<OnionRouteHop>,
    /// Signed descriptor for the selected exit.
    exit: OnionExitDescriptor,
}

impl OnionRoute {
    /// Build a route after proving the hop and exit fields agree.
    ///
    /// Invariant: `hops == encryption_hops.map(|hop| hop.did)`, no DID repeats, and the last hop is
    /// the selected exit descriptor.
    ///
    /// Invariant: `service` is canonical, so route/payload service equality is ordinary value
    /// equality over [`OnionServiceName`], not caller-dependent string normalization.
    pub(crate) fn new(
        service: OnionServiceName,
        encryption_hops: Vec<OnionRouteHop>,
        exit: OnionExitDescriptor,
    ) -> Result<Self> {
        validate_route_hops(&service, &encryption_hops, &exit)?;
        let hops = encryption_hops
            .iter()
            .map(|hop| hop.did)
            .collect::<Vec<_>>();
        Ok(Self {
            service,
            hops,
            encryption_hops,
            exit,
        })
    }

    /// Return the service used to select this route.
    pub fn service(&self) -> &str {
        self.service.as_str()
    }

    /// Return the canonical service name used to select this route.
    pub fn service_name(&self) -> &OnionServiceName {
        &self.service
    }

    /// Return the ordered route DIDs, ending with the exit DID.
    pub fn hops(&self) -> &[Did] {
        self.hops.as_slice()
    }

    /// Return the ordered encrypted hops, ending with the exit hop.
    pub(crate) fn encryption_hops(&self) -> &[OnionRouteHop] {
        self.encryption_hops.as_slice()
    }

    /// Return the selected exit descriptor.
    pub fn exit(&self) -> &OnionExitDescriptor {
        &self.exit
    }

    /// Return the selected exit DID.
    pub fn exit_did(&self) -> Did {
        self.exit.did
    }
}

pub(crate) trait RouteEntropy {
    fn next_u64(&mut self) -> u64;
}

pub(crate) struct SystemRouteEntropy;

impl SystemRouteEntropy {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl RouteEntropy for SystemRouteEntropy {
    fn next_u64(&mut self) -> u64 {
        rand::random()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OnionRouteCandidates {
    pub(in crate::onion) relays: Vec<OnionRouteHop>,
    pub(in crate::onion) exits: Vec<OnionExitDescriptor>,
}

impl OnionRouteCandidates {
    pub(crate) fn from_validated_descriptors(
        local: Did,
        service: &OnionServiceName,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        exits: impl IntoIterator<Item = OnionExitDescriptor>,
    ) -> Self {
        let relays = online_nodes
            .into_iter()
            .filter(has_onion_relay_capability)
            .map(|descriptor| OnionRouteHop::new(descriptor.did, descriptor.session_public_key))
            .filter(|hop| hop.did != local)
            .map(|hop| (hop.did, hop))
            .collect::<BTreeMap<_, _>>();

        let exits = exits
            .into_iter()
            .filter(|descriptor| descriptor.offers_service(service.as_str()))
            .filter(|descriptor| descriptor.did != local)
            .collect::<Vec<_>>();

        Self {
            relays: relays.into_values().collect(),
            exits,
        }
    }
}

/// Select an onion route from live presence and exit descriptors.
///
/// Invariant: the returned hop list contains no duplicate DID and always ends
/// in a descriptor from the exit registry.
pub fn select_onion_route(
    local: Did,
    dht_protocol: DhtProtocolMode,
    now_ms: u128,
    request: &OnionRouteRequest,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
) -> Result<OnionRoute> {
    let candidates = OnionRouteCandidates {
        relays: eligible_relay_dids(dht_protocol, now_ms, local, online_nodes)
            .into_iter()
            .collect(),
        exits: eligible_exits(
            dht_protocol.network_id,
            now_ms,
            request.service_name(),
            exits,
        )
        .into_iter()
        .filter(|descriptor| descriptor.did != local)
        .collect(),
    };
    select_onion_route_from_candidates(
        request,
        candidates,
        qualities,
        &mut SystemRouteEntropy::new(),
    )
}

pub(crate) fn select_onion_route_from_candidates(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
) -> Result<OnionRoute> {
    let target_hop_count = request.target_hop_count();
    if target_hop_count == 0 || target_hop_count > usize::from(MAX_ONION_CIRCUIT_HOPS) {
        return Err(Error::OnionRouteError(
            OnionRouteError::HopCountOutOfBounds {
                hop_count: target_hop_count,
                max_hops: MAX_ONION_CIRCUIT_HOPS,
            },
        ));
    }

    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let mut exit_candidates = candidates.exits;
    let exit_dids = exit_candidates
        .iter()
        .map(|descriptor| descriptor.did)
        .collect::<Vec<_>>();
    let exit_index =
        pick_weighted_index(&exit_dids, &quality_by_did, entropy).ok_or_else(|| {
            Error::OnionRouteError(OnionRouteError::NoLiveExit {
                service: request.service().to_string(),
            })
        })?;
    let exit = exit_candidates.remove(exit_index);
    let exit_did = exit.did;

    let mut relay_candidates = candidates
        .relays
        .into_iter()
        .filter(|hop| hop.did != exit_did)
        .collect::<Vec<_>>();
    let relay_hops_needed = target_hop_count.saturating_sub(1);
    let mut selected_relays = Vec::with_capacity(relay_hops_needed);
    while selected_relays.len() < relay_hops_needed {
        let Some(next_index) = pick_weighted_hop_index(&relay_candidates, &quality_by_did, entropy)
        else {
            break;
        };
        selected_relays.push(relay_candidates.remove(next_index));
    }

    if selected_relays.len() < relay_hops_needed && !request.allow_short_paths {
        return Err(Error::OnionRouteError(OnionRouteError::NotEnoughRelays {
            hop_count: target_hop_count,
        }));
    }

    let mut encryption_hops = selected_relays;
    encryption_hops.push(OnionRouteHop::new(exit_did, exit.session_public_key));
    OnionRoute::new(request.service.clone(), encryption_hops, exit)
}

fn pick_weighted_hop_index(
    hops: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<usize> {
    let dids = hops.iter().map(|hop| hop.did).collect::<Vec<_>>();
    pick_weighted_index(&dids, quality_by_did, entropy)
}

fn pick_weighted_index(
    dids: &[Did],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<usize> {
    let total_weight = dids
        .iter()
        .map(|did| quality_weight(quality_by_did.get(did).copied()))
        .sum::<u64>();
    if total_weight == 0 {
        return None;
    }

    let mut roll = entropy.next_u64() % total_weight;
    for (index, did) in dids.iter().enumerate() {
        let weight = quality_weight(quality_by_did.get(did).copied());
        if roll < weight {
            return Some(index);
        }
        roll -= weight;
    }
    None
}

fn quality_weight(quality: Option<PeerQuality>) -> u64 {
    match quality {
        Some(PeerQuality::Healthy) => 8,
        Some(PeerQuality::Unknown) | None => 4,
        Some(PeerQuality::Degraded) => 1,
    }
}

fn eligible_exits(
    network_id: u32,
    now_ms: u128,
    service: &OnionServiceName,
    exits: impl IntoIterator<Item = OnionExitDescriptor>,
) -> Vec<OnionExitDescriptor> {
    OnionExitDescriptor::latest_valid_by_service_did(exits, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_network(network_id))
        .filter(|descriptor| descriptor.offers_service(service.as_str()))
        .collect()
}

fn eligible_relay_dids(
    dht_protocol: DhtProtocolMode,
    now_ms: u128,
    local: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> Vec<OnionRouteHop> {
    OnlineNodeDescriptor::latest_valid_by_did(online_nodes, now_ms, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_dht_protocol(dht_protocol))
        .filter(has_onion_relay_capability)
        .map(|descriptor| OnionRouteHop::new(descriptor.did, descriptor.session_public_key))
        .filter(|hop| hop.did != local)
        .map(|hop| (hop.did, hop))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

fn has_onion_relay_capability(descriptor: &OnlineNodeDescriptor) -> bool {
    descriptor
        .capabilities
        .iter()
        .any(|capability| capability == ONION_RELAY_CAPABILITY)
}

fn has_duplicate_dids(hops: &[Did]) -> bool {
    let mut seen = BTreeSet::new();
    hops.iter().any(|did| !seen.insert(*did))
}

fn validate_route_hops(
    service: &OnionServiceName,
    encryption_hops: &[OnionRouteHop],
    exit: &OnionExitDescriptor,
) -> Result<()> {
    if encryption_hops.is_empty() || encryption_hops.len() > usize::from(MAX_ONION_CIRCUIT_HOPS) {
        return Err(Error::OnionRouteError(
            OnionRouteError::HopCountOutOfBounds {
                hop_count: encryption_hops.len(),
                max_hops: MAX_ONION_CIRCUIT_HOPS,
            },
        ));
    }
    let Some(last) = encryption_hops.last() else {
        return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
    };
    if last.did != exit.did || last.session_public_key != exit.session_public_key {
        return Err(Error::OnionRouteError(OnionRouteError::ExitHopMismatch));
    }
    let hops = encryption_hops
        .iter()
        .map(|hop| hop.did)
        .collect::<Vec<_>>();
    if has_duplicate_dids(&hops) {
        return Err(Error::OnionRouteError(OnionRouteError::DuplicateRouteHops));
    }
    if !exit.offers_service(service.as_str()) {
        return Err(Error::OnionRouteError(OnionRouteError::ExitServiceMismatch));
    }
    Ok(())
}

fn parse_route_service(service: impl AsRef<str>) -> Result<OnionServiceName> {
    let service = service.as_ref();
    if service.trim().is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::EmptyRouteService));
    }
    OnionServiceName::parse(service)
}

#[cfg(test)]
mod tests;
