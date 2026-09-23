//! Onion route selection: guard-closed loops over registered symbols (#834 D2, D4, D5, L7).
//!
//! A route request names a pipeline of symbols, and selection places it on a loop of the pipeline's
//! shape (`loop_shape`): every position takes one node registering that position's symbol,
//!
//! ```text
//! relay position (guard included)  ← OnlineNodeDescriptor with capabilities.onion_relay = e_n
//! symbol position hₖ               ← descriptor of fₖ under ONION_EXITS_TOPIC, epoch e_n
//! ```
//!
//! Laws:
//!
//! - **Registration** (D2). A symbol registrant is eligible only if the same process — equal DID,
//!   session key and process epoch — registers `relay`: `Σ_n ≠ ∅ ⇒ relay ∈ Σ_n`. A symbol
//!   descriptor whose node registers no `relay` has no epoch to agree with, and one whose epoch
//!   differs from the node's current relay epoch is stale; both are rejected.
//! - **Fail closed** (D5). With `R` the eligible relay registrants, selection fails unless
//!   `|R| ≥ H − 1`; no route is ever shortened, since a short path is a distinguishable segment
//!   length.
//! - **Guard closure** (L7). The guard `g` is drawn once from the permitted first hops and closes
//!   the loop at position `H`; the other `H − 2` hops are pairwise distinct and distinct from `g`
//!   (`has_duplicate_dids`).
//!
//! Until #834 Phase 2a-4 the data plane consumes a route through the Phase 1 view
//! `OnionRoute::positions`: the loop's forward prefix `g, r₀,₂, h₁ = relay^s ⋙ (s, ā)`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::loop_shape::OnionLoop;
use super::loop_shape::OnionLoopRole;
use super::loop_shape::OnionLoopShape;
use super::loop_shape::ONION_SEGMENT_RELAYS;
use super::pipeline::OnionPipeline;
use super::pipeline::OnionSymbolWord;
use super::OnionExitDescriptor;
use super::OnionProcessEpoch;
use super::OnionRouteError;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;
use crate::online::OnlineNodeDescriptor;

/// Route-building request for an onion circuit.
///
/// The request names one world-facing symbol, which stands alone in its pipeline (#834 D4a), so
/// the loop has `n = 1` symbol hop and `H = (s + 1) + s` positions; the length is fixed by the
/// pipeline, never by the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRouteRequest {
    /// World-facing symbol of the pipeline.
    pub service: OnionServiceName,
}

impl OnionRouteRequest {
    /// Build a route request from an already canonical service name.
    pub const fn from_service_name(service: OnionServiceName) -> Self {
        Self { service }
    }

    /// Return the canonical service selected by this request.
    pub fn service(&self) -> &str {
        self.service.as_str()
    }

    /// Return the canonical service name selected by this request.
    pub(crate) const fn service_name(&self) -> &OnionServiceName {
        &self.service
    }
}

/// One hop selected for encrypted onion routing: a node process registering a symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionRouteHop {
    /// Hop DID.
    pub did: Did,
    /// Hop session public key used for ElGamal-AEAD layers.
    pub delegatee_public_key: PublicKey<33>,
    /// Process epoch `e_n` of the hop's registration (#834 D2).
    pub process_epoch: OnionProcessEpoch,
}

impl OnionRouteHop {
    /// Build a route hop from its DID, session public key and process epoch.
    pub const fn new(
        did: Did,
        delegatee_public_key: PublicKey<33>,
        process_epoch: OnionProcessEpoch,
    ) -> Self {
        Self {
            did,
            delegatee_public_key,
            process_epoch,
        }
    }

    /// Return the hop that registers the symbol of `descriptor`.
    pub const fn of_symbol(descriptor: &OnionExitDescriptor) -> Self {
        Self::new(
            descriptor.did,
            descriptor.delegatee_public_key,
            descriptor.process_epoch,
        )
    }
}

/// Selected onion route: a guard-closed loop whose symbol hop evaluates the route's service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionRoute {
    /// World-facing symbol of the route's pipeline.
    service: OnionServiceName,
    /// Hop assignment of the loop, one hop per position.
    hops: OnionLoop<OnionRouteHop>,
    /// Signed descriptor registering `service` at the symbol hop.
    exit: OnionExitDescriptor,
}

impl OnionRoute {
    /// Build a route after proving the loop and exit fields agree.
    ///
    /// Invariant: the loop has one symbol hop, which is the process registering `service` in
    /// `exit` (equal DID, session key and epoch), and no DID repeats except the guard at positions
    /// `1` and `H` (L7).
    ///
    /// Invariant: `service` is canonical, so route/payload service equality is ordinary value
    /// equality over [`OnionServiceName`], not caller-dependent string normalization.
    pub fn new(
        service: OnionServiceName,
        hops: OnionLoop<OnionRouteHop>,
        exit: OnionExitDescriptor,
    ) -> Result<Self> {
        validate_route_hops(&service, &hops, &exit)?;
        Ok(Self {
            service,
            hops,
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

    /// Return the loop's hop assignment, positions `1 … H`.
    pub fn hops(&self) -> &OnionLoop<OnionRouteHop> {
        &self.hops
    }

    /// Return the Phase 1 view of the loop: its forward prefix `g, r₀,₂ … r₀,ₛ, h₁`, one encrypted
    /// hop per position of [`Self::word`]. The data plane seals this path until #834 Phase 2a-4
    /// consumes the whole loop.
    pub(crate) fn positions(&self) -> OnionPipeline<OnionRouteHop> {
        OnionPipeline::new(
            self.hops
                .positions()
                .take(ONION_SEGMENT_RELAYS)
                .copied()
                .collect(),
            OnionRouteHop::of_symbol(&self.exit),
        )
    }

    /// Return the symbol word `relay^s ⋙ service` of the Phase 1 view.
    pub fn word(&self) -> OnionSymbolWord {
        OnionSymbolWord::new(ONION_SEGMENT_RELAYS, self.service.clone())
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

/// Source of the uniform draws behind weighted hop selection, injected for determinism.
pub(crate) trait RouteEntropy {
    /// Draw the next uniform 64-bit value.
    fn next_u64(&mut self) -> u64;
}

/// Route entropy drawn from the thread-local CSPRNG.
pub(crate) struct SystemRouteEntropy;

impl SystemRouteEntropy {
    /// Build the system entropy source.
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl RouteEntropy for SystemRouteEntropy {
    fn next_u64(&mut self) -> u64 {
        rand::random()
    }
}

/// Registrants eligible for one route: relay registrants and the registrants of its symbol.
///
/// Invariant: every exit's hop ([`OnionRouteHop::of_symbol`]) is an element of `relays`, and no
/// DID in either is the local node.
#[derive(Clone, Debug)]
pub(crate) struct OnionRouteCandidates {
    pub(in crate::onion) relays: Vec<OnionRouteHop>,
    pub(in crate::onion) exits: Vec<OnionExitDescriptor>,
}

impl OnionRouteCandidates {
    /// Collect the live registrants of `relay` and of `service` in the local DHT protocol mode.
    ///
    /// A `service` registrant is kept only if the same process registers `relay` (D2): its hop
    /// `(did, session key, e_n)` must be one of the relay hops, which rejects a symbol descriptor
    /// without a relay epoch and one whose epoch is stale.
    pub(crate) fn from_validated_descriptors(
        local: Did,
        dht_protocol: DhtProtocolMode,
        now_ms: u128,
        service: &OnionServiceName,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        exits: impl IntoIterator<Item = OnionExitDescriptor>,
    ) -> Self {
        let relays = eligible_relays(dht_protocol, now_ms, local, online_nodes);
        let exits = eligible_exits(dht_protocol.network_id, now_ms, service, exits)
            .into_iter()
            .filter(|descriptor| relays.contains(&OnionRouteHop::of_symbol(descriptor)))
            .collect();

        Self { relays, exits }
    }
}

/// Select a route for `request` from prevalidated candidates and an explicit guard policy.
///
/// The request's pipeline is its one world-facing symbol, registered by `candidates.exits`; the
/// loop is drawn by [`select_loop`] and its symbol hop resolved back to the registering
/// descriptor. Callers must state the guard policy explicitly, so a permissive default cannot
/// bypass entry-guard policy.
pub(crate) fn select_onion_route_from_candidates(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let service = request.service_name();
    let no_live_exit = || {
        Error::OnionRouteError(OnionRouteError::NoLiveExit {
            service: service.as_str().to_string(),
        })
    };
    if candidates.exits.is_empty() {
        return Err(no_live_exit());
    }
    let registrants = [candidates
        .exits
        .iter()
        .map(|descriptor| descriptor.did)
        .collect::<BTreeSet<_>>()];
    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let hops = select_loop(
        &registrants,
        &candidates.relays,
        &quality_by_did,
        entropy,
        guard_permitted,
    )?;
    let symbol_hop = hops
        .symbol(OnionLoopShape::SESSION.symbols())
        .map(|hop| hop.did)
        .ok_or_else(no_live_exit)?;
    let exit = candidates
        .exits
        .into_iter()
        .find(|descriptor| descriptor.did == symbol_hop)
        .ok_or_else(no_live_exit)?;
    OnionRoute::new(service.clone(), hops, exit)
}

/// Draw a loop for the pipeline whose `k`-th symbol is registered by `registrants[k − 1]`.
///
/// Every hop comes from `relays`, the relay registrants; a symbol registrant outside `relays`
/// registers no `relay` and is not eligible (D2). With `Sₖ` the eligible registrants of symbol
/// `k` and `SDR(F, T)` Hall's condition that the family `F`, less the taken hops `T`, still has
/// distinct representatives:
///
/// ```text
/// shape  ← OnionLoopShape::new(n)                          n ∉ [1, 4]      → LoopSymbolsOutOfBounds
/// |R| < H − 1                                                               → NotEnoughLoopHops
/// ¬SDR(S₁ … Sₙ, ∅)                                                          → NoDistinctSymbolHops
/// g      ← draw { r ∈ R | guard_permitted(r) ∧ SDR(S₁ … Sₙ, {r}) }      none → NoPermittedFirstHop
/// for k = 1 … n:
///   hₖ   ← draw { d ∈ Sₖ ∖ T | SDR(Sₖ₊₁ … Sₙ, T ∪ {d}) },  T ← T ∪ {hₖ}
/// rest   ← H − 2 − n draws without replacement from R ∖ T
/// loop   = shape.try_label(Guard ↦ g, Symbol(k) ↦ hₖ, Relay ↦ next of rest)
/// ```
///
/// Every draw is weighted by peer quality. The SDR guard on each draw makes the greedy order
/// complete: a draw never strands a later symbol position, so selection fails only when no loop
/// exists, and `|R| ≥ H − 1` leaves at least `H − 2 − n` relays once `g` and the `hₖ` are taken.
fn select_loop(
    registrants: &[BTreeSet<Did>],
    relays: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionLoop<OnionRouteHop>> {
    let shape = OnionLoopShape::new(registrants.len())?;
    let hop_by_did = relays
        .iter()
        .map(|hop| (hop.did, *hop))
        .collect::<BTreeMap<_, _>>();
    if hop_by_did.len() < shape.distinct_hops() {
        return Err(Error::OnionRouteError(OnionRouteError::NotEnoughLoopHops {
            required: shape.distinct_hops(),
            eligible: hop_by_did.len(),
        }));
    }
    let symbols = registrants
        .iter()
        .map(|registrant| {
            registrant
                .iter()
                .copied()
                .filter(|did| hop_by_did.contains_key(did))
                .collect::<BTreeSet<_>>()
        })
        .collect::<Vec<_>>();
    let no_symbol_hops = || Error::OnionRouteError(OnionRouteError::NoDistinctSymbolHops);
    if !admits_distinct_symbol_hops(symbols.as_slice(), &BTreeSet::new()) {
        return Err(no_symbol_hops());
    }

    let guard = draw_weighted(
        hop_by_did.keys().copied().filter(|did| {
            guard_permitted(*did)
                && admits_distinct_symbol_hops(symbols.as_slice(), &BTreeSet::from([*did]))
        }),
        quality_by_did,
        entropy,
    )
    .ok_or(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))?;
    let mut taken = BTreeSet::from([guard]);
    let mut symbol_hops = Vec::with_capacity(symbols.len());
    for (k, registrant) in symbols.iter().enumerate() {
        let later = symbols.get(k + 1..).unwrap_or_default();
        let hop = draw_weighted(
            registrant.difference(&taken).copied().filter(|did| {
                let mut after = taken.clone();
                after.insert(*did);
                admits_distinct_symbol_hops(later, &after)
            }),
            quality_by_did,
            entropy,
        )
        .ok_or_else(no_symbol_hops)?;
        taken.insert(hop);
        symbol_hops.push(hop);
    }
    let not_enough_hops = || {
        Error::OnionRouteError(OnionRouteError::NotEnoughLoopHops {
            required: shape.distinct_hops(),
            eligible: hop_by_did.len(),
        })
    };
    let hop_of = |did: Did| hop_by_did.get(&did).copied().ok_or_else(not_enough_hops);
    shape.try_label(|role| match role {
        OnionLoopRole::Guard => hop_of(guard),
        OnionLoopRole::Symbol(k) => k
            .checked_sub(1)
            .and_then(|index| symbol_hops.get(index))
            .copied()
            .ok_or_else(no_symbol_hops)
            .and_then(hop_of),
        OnionLoopRole::Relay => {
            let relay = draw_weighted(
                hop_by_did
                    .keys()
                    .copied()
                    .filter(|did| !taken.contains(did)),
                quality_by_did,
                entropy,
            )
            .ok_or_else(not_enough_hops)?;
            taken.insert(relay);
            hop_of(relay)
        }
    })
}

/// Hall's condition for the open symbol positions: every nonempty subfamily `J` of `symbols`,
/// less the `taken` hops, covers at least `|J|` hops,
///
/// ```text
/// SDR(S, T)  ⇔  ∀ J ⊆ S, J ≠ ∅.  |⋃J ∖ T| ≥ |J|,
/// ```
///
/// which holds exactly when the positions can still take pairwise distinct untaken hops. The
/// family has at most `MAX_ONION_LOOP_SYMBOLS` members, so at most 15 subfamilies are checked.
fn admits_distinct_symbol_hops(symbols: &[BTreeSet<Did>], taken: &BTreeSet<Did>) -> bool {
    (1..1_usize << symbols.len()).all(|family| {
        let covered = symbols
            .iter()
            .enumerate()
            .filter(|(index, _)| (family >> index) & 1 == 1)
            .flat_map(|(_, registrant)| registrant.difference(taken))
            .collect::<BTreeSet<_>>();
        covered.len() >= family.count_ones() as usize
    })
}

/// Draw one DID from `candidates` with probability proportional to its quality weight, or `None`
/// when there is no candidate.
fn draw_weighted(
    candidates: impl Iterator<Item = Did>,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<Did> {
    let candidates = candidates.collect::<Vec<_>>();
    pick_weighted_index(candidates.as_slice(), quality_by_did, entropy)
        .and_then(|index| candidates.get(index).copied())
}

/// Pick an index of `dids` with probability proportional to its quality weight, or `None` when
/// the total weight is zero.
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

/// Selection weight of a peer quality: healthy peers are preferred, degraded ones kept last.
fn quality_weight(quality: Option<PeerQuality>) -> u64 {
    match quality {
        Some(PeerQuality::Healthy) => 8,
        Some(PeerQuality::Unknown) | None => 4,
        Some(PeerQuality::Degraded) => 1,
    }
}

/// Return the newest live descriptor per DID that registers `service`.
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

/// Return the relay hop of every remote node whose newest live descriptor in the local DHT
/// protocol mode registers `relay`, ordered by DID, at the epoch it registers it with.
fn eligible_relays(
    dht_protocol: DhtProtocolMode,
    now_ms: u128,
    local: Did,
    online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
) -> Vec<OnionRouteHop> {
    OnlineNodeDescriptor::latest_valid_by_did(online_nodes, now_ms, dht_protocol.network_id, false)
        .into_iter()
        .filter(|descriptor| descriptor.matches_dht_protocol(dht_protocol))
        .filter(|descriptor| descriptor.did != local)
        .filter_map(|descriptor| {
            descriptor.capabilities.onion_relay.map(|epoch| {
                OnionRouteHop::new(descriptor.did, descriptor.delegatee_public_key, epoch)
            })
        })
        .map(|hop| (hop.did, hop))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect()
}

/// Return whether a DID repeats on the loop's open path `1 … H − 1` (L7).
///
/// The guard's second occurrence, at position `H`, is structural and the only repetition a loop
/// admits: a loop without duplicates holds every DID once, except the guard exactly twice.
fn has_duplicate_dids(hops: &OnionLoop<OnionRouteHop>) -> bool {
    let mut seen = BTreeSet::new();
    hops.open_path().any(|hop| !seen.insert(hop.did))
}

/// Validate a route's loop against its pipeline and symbol descriptor (see [`OnionRoute::new`]).
fn validate_route_hops(
    service: &OnionServiceName,
    hops: &OnionLoop<OnionRouteHop>,
    exit: &OnionExitDescriptor,
) -> Result<()> {
    if hops.shape() != OnionLoopShape::SESSION {
        return Err(Error::OnionRouteError(OnionRouteError::LoopShapeMismatch {
            expected: OnionLoopShape::SESSION.symbols(),
            actual: hops.shape().symbols(),
        }));
    }
    if hops.symbol(OnionLoopShape::SESSION.symbols()) != Some(&OnionRouteHop::of_symbol(exit)) {
        return Err(Error::OnionRouteError(OnionRouteError::ExitHopMismatch));
    }
    if has_duplicate_dids(hops) {
        return Err(Error::OnionRouteError(OnionRouteError::DuplicateRouteHops));
    }
    if !exit.offers_service(service.as_str()) {
        return Err(Error::OnionRouteError(OnionRouteError::ExitServiceMismatch));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
