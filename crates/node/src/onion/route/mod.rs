//! Onion route selection.
//!
//! A route request denotes a symbol word `σ = relay^{k−1} ⋙ s` for a target of `k` hops, and
//! selection assigns one node to each position from the nodes registering that position's symbol;
//! the selected route is the hop assignment `h₀ … hₙ` of the word it was selected for:
//!
//! ```text
//! relay position        ← OnlineNodeDescriptor with ONION_RELAY_CAPABILITY   (#834 D2)
//! world-facing s        ← OnionExitDescriptor offering s under ONION_EXITS_TOPIC
//! ```
//!
//! A short path drops relay positions only; by `relay ⋙ f = f` (#834 L1) this never changes the
//! pipeline's denotation.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::circuit::MAX_ONION_CIRCUIT_HOPS;
use super::pipeline::OnionPipeline;
use super::pipeline::OnionSymbolWord;
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
///
/// The pair `(service, hop_count)` denotes the requested symbol word `relay^{k−1} ⋙ service`; route
/// selection reads that word once the hop bound admits `k`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRouteRequest {
    /// World-facing symbol of the last position.
    pub service: OnionServiceName,
    /// Desired hop count including the exit. `0` uses [`DEFAULT_ONION_ROUTE_HOPS`].
    pub hop_count: usize,
    /// Whether a route may be shorter than `hop_count` when the network is too small.
    pub allow_short_paths: bool,
}

impl OnionRouteRequest {
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

    /// Return the requested symbol word `relay^{k−1} ⋙ service`.
    ///
    /// Post: `1 ≤ k ≤ MAX_ONION_CIRCUIT_HOPS`.
    fn word(&self) -> Result<OnionSymbolWord> {
        let target_hop_count = self.target_hop_count();
        if target_hop_count > usize::from(MAX_ONION_CIRCUIT_HOPS) {
            return Err(Error::OnionRouteError(
                OnionRouteError::HopCountOutOfBounds {
                    hop_count: target_hop_count,
                    max_hops: MAX_ONION_CIRCUIT_HOPS,
                },
            ));
        }
        Ok(OnionSymbolWord::new(
            target_hop_count.saturating_sub(1),
            self.service.clone(),
        ))
    }
}

/// One hop selected for encrypted onion routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionRouteHop {
    /// Hop DID.
    pub did: Did,
    /// Hop session public key used for ElGamal-AEAD layers.
    pub delegatee_public_key: PublicKey<33>,
}

impl OnionRouteHop {
    /// Build a route hop from its DID and session public key.
    pub const fn new(did: Did, delegatee_public_key: PublicKey<33>) -> Self {
        Self {
            did,
            delegatee_public_key,
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
    /// Hop assignment `h₀ … hₙ` of the route's symbol word, ending with the exit hop.
    positions: OnionPipeline<OnionRouteHop>,
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
        let mut relays = encryption_hops;
        let Some(terminal) = relays.pop() else {
            return Err(Error::OnionRouteError(OnionRouteError::RouteHasNoHops));
        };
        Ok(Self {
            service,
            hops,
            positions: OnionPipeline::new(relays, terminal),
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

    /// Return the hop assignment `h₀ … hₙ`, one encrypted hop per position of [`Self::word`].
    pub(crate) const fn positions(&self) -> &OnionPipeline<OnionRouteHop> {
        &self.positions
    }

    /// Return the symbol word `relay^n ⋙ service` this route was selected for.
    pub fn word(&self) -> OnionSymbolWord {
        OnionSymbolWord::new(self.positions.relays().len(), self.service.clone())
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
        dht_protocol: DhtProtocolMode,
        now_ms: u128,
        service: &OnionServiceName,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        exits: impl IntoIterator<Item = OnionExitDescriptor>,
    ) -> Self {
        let relays = eligible_relay_dids(dht_protocol, now_ms, local, online_nodes);
        let exits = eligible_exits(dht_protocol.network_id, now_ms, service, exits)
            .into_iter()
            .filter(|descriptor| descriptor.did != local)
            .collect();

        Self { relays, exits }
    }
}

/// Select a route from prevalidated candidates and explicit first-hop policies.
///
/// Each position of the requested pipeline takes one node registering its symbol: relay
/// positions draw from `candidates.relays`, the world-facing position from `candidates.exits`.
///
/// Invariant: the returned hop list contains no duplicate DID and always ends
/// in a descriptor from the exit registry. Callers must explicitly state both
/// the relay-first-hop and direct-exit policies so a permissive default cannot
/// silently bypass entry-guard policy.
pub(crate) fn select_onion_route_from_candidates_with_first_hop_policy(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
    first_relay_hop_permitted: impl Fn(Did) -> bool,
    direct_exit_permitted: impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let word = request.word()?;
    let target_hop_count = word.hop_count();

    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let mut exit_candidates = candidates.exits;
    let first_relay_hop_permitted = &first_relay_hop_permitted;
    let direct_exit_permitted = &direct_exit_permitted;
    if exit_candidates.is_empty() {
        return Err(Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service: word.terminal().as_str().to_string(),
        }));
    }
    if word.relays() == 0 {
        return select_direct_exit_route(
            word.terminal(),
            exit_candidates,
            &quality_by_did,
            entropy,
            direct_exit_permitted,
        );
    }

    let mut relay_candidates = candidates.relays.into_iter().collect::<Vec<_>>();
    let relay_hops_needed = word.relays();
    let mut selected_relays = Vec::with_capacity(relay_hops_needed);
    if relay_hops_needed > 0 {
        let has_relay_candidates = !relay_candidates.is_empty();
        let Some(first_index) =
            pick_weighted_hop_index_where(&relay_candidates, &quality_by_did, entropy, |did| {
                first_relay_hop_permitted(did)
                    && route_can_still_select_exit(&selected_relays, did, &exit_candidates)
            })
        else {
            if request.allow_short_paths {
                return select_direct_exit_route(
                    word.terminal(),
                    exit_candidates,
                    &quality_by_did,
                    entropy,
                    direct_exit_permitted,
                );
            }
            let error = if has_relay_candidates {
                OnionRouteError::NoPermittedFirstHop
            } else {
                OnionRouteError::NotEnoughRelays {
                    hop_count: target_hop_count,
                }
            };
            return Err(Error::OnionRouteError(error));
        };
        selected_relays.push(relay_candidates.remove(first_index));
        while selected_relays.len() < relay_hops_needed {
            let Some(next_index) =
                pick_weighted_hop_index_where(&relay_candidates, &quality_by_did, entropy, |did| {
                    route_can_still_select_exit(&selected_relays, did, &exit_candidates)
                })
            else {
                break;
            };
            selected_relays.push(relay_candidates.remove(next_index));
        }
    }

    if selected_relays.len() < relay_hops_needed && !request.allow_short_paths {
        return Err(Error::OnionRouteError(OnionRouteError::NotEnoughRelays {
            hop_count: target_hop_count,
        }));
    }

    let exit_index =
        pick_weighted_exit_index_where(&exit_candidates, &quality_by_did, entropy, |did| {
            !route_already_contains_did(&selected_relays, did)
        })
        .ok_or_else(|| {
            Error::OnionRouteError(OnionRouteError::NoLiveExit {
                service: word.terminal().as_str().to_string(),
            })
        })?;
    let exit = exit_candidates.remove(exit_index);
    let exit_did = exit.did;
    let mut encryption_hops = selected_relays;
    encryption_hops.push(OnionRouteHop::new(exit_did, exit.delegatee_public_key));
    OnionRoute::new(word.terminal().clone(), encryption_hops, exit)
}

/// Select a one-hop route whose only position is the world-facing `service`.
fn select_direct_exit_route(
    service: &OnionServiceName,
    mut exits: Vec<OnionExitDescriptor>,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    first_hop_permitted: &impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let exit_index =
        pick_weighted_exit_index_where(&exits, quality_by_did, entropy, first_hop_permitted)
            .ok_or(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))?;
    let exit = exits.remove(exit_index);
    let encryption_hops = vec![OnionRouteHop::new(exit.did, exit.delegatee_public_key)];
    OnionRoute::new(service.clone(), encryption_hops, exit)
}

fn route_can_still_select_exit(
    selected_relays: &[OnionRouteHop],
    candidate_relay: Did,
    exits: &[OnionExitDescriptor],
) -> bool {
    exits.iter().any(|exit| {
        exit.did != candidate_relay && !route_already_contains_did(selected_relays, exit.did)
    })
}

fn route_already_contains_did(selected_relays: &[OnionRouteHop], did: Did) -> bool {
    selected_relays.iter().any(|hop| hop.did == did)
}

fn pick_weighted_hop_index_where(
    hops: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    permitted: impl Fn(Did) -> bool,
) -> Option<usize> {
    let eligible = hops
        .iter()
        .enumerate()
        .filter_map(|(index, hop)| permitted(hop.did).then_some((index, hop.did)))
        .collect::<Vec<_>>();
    pick_weighted_candidate_index(eligible, quality_by_did, entropy)
}

fn pick_weighted_exit_index_where(
    exits: &[OnionExitDescriptor],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    permitted: impl Fn(Did) -> bool,
) -> Option<usize> {
    let eligible = exits
        .iter()
        .enumerate()
        .filter_map(|(index, descriptor)| {
            permitted(descriptor.did).then_some((index, descriptor.did))
        })
        .collect::<Vec<_>>();
    pick_weighted_candidate_index(eligible, quality_by_did, entropy)
}

fn pick_weighted_candidate_index(
    eligible: Vec<(usize, Did)>,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<usize> {
    let dids = eligible.iter().map(|(_, did)| *did).collect::<Vec<_>>();
    let selected = pick_weighted_index(&dids, quality_by_did, entropy)?;
    eligible.into_iter().nth(selected).map(|(index, _)| index)
}

pub(crate) fn pick_weighted_index(
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
    OnionExitDescriptor::latest_valid_by_service_did(exits, now_ms, network_id, false)
        .into_iter()
        .filter(|descriptor| descriptor.offers_service(service.as_str()))
        .collect()
}

fn eligible_relay_dids(
    dht_protocol: DhtProtocolMode,
    now_ms: u128,
    local: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> Vec<OnionRouteHop> {
    OnlineNodeDescriptor::latest_valid_by_did(online_nodes, now_ms, dht_protocol.network_id, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_dht_protocol(dht_protocol))
        .filter(has_onion_relay_capability)
        .map(|descriptor| OnionRouteHop::new(descriptor.did, descriptor.delegatee_public_key))
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
    if last.did != exit.did || last.delegatee_public_key != exit.delegatee_public_key {
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

#[cfg(test)]
mod tests;
