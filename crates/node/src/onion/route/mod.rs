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
//! Until #834 Phase 2a-4 the data plane consumes only the loop's forward prefix
//! [`OnionRoute::circuit_hops`], `g, r₀,₂, h₁ = relay^s ⋙ (s, ā)`, and answers along its reverse.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::iter;

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::pipeline::OnionPipeline;
use super::pipeline::OnionSymbolWord;
use super::OnionExitDescriptor;
use super::OnionLoop;
use super::OnionLoopRelay;
use super::OnionLoopShape;
use super::OnionPending;
use super::OnionPipelineSymbols;
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
    pub(crate) fn new(
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

    /// Return the hops the circuit uses: the loop's forward prefix `g, r₀,₂ … r₀,ₛ, h₁`.
    ///
    /// Until #834 Phase 2a-4 the data plane seals only this prefix and answers along its reverse;
    /// the return segment of [`Self::hops`] is selected but unused.
    pub fn circuit_hops(&self) -> impl Iterator<Item = &OnionRouteHop> {
        self.hops
            .forward_prefix()
            .chain(iter::once(self.hops.terminal()))
    }

    /// Return [`Self::circuit_hops`] as the Phase 1 pipeline the data plane seals: the relay
    /// positions before `h₁`, then the terminal `h₁ = hₙ` (the loop has `n = 1`).
    pub(crate) fn forward_path(&self) -> OnionPipeline<OnionRouteHop> {
        OnionPipeline::new(
            self.hops.forward_prefix().copied().collect(),
            *self.hops.terminal(),
        )
    }

    /// Return the symbol word `relay^s ⋙ service` of [`Self::circuit_hops`].
    pub fn word(&self) -> OnionSymbolWord {
        OnionSymbolWord::new(self.hops.forward_prefix().count(), self.service.clone())
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

/// Live registrants for one route: relay registrants and the registrants of its symbol.
///
/// Invariant: `relays` holds one hop per DID, and no DID in either field is the local node.
/// `exits` are not yet matched against `relays`: that is D2's admission, made by selection so
/// that each rejection is reported with its cause.
#[derive(Clone, Debug)]
pub(crate) struct OnionRouteCandidates {
    pub(in crate::onion) relays: Vec<OnionRouteHop>,
    pub(in crate::onion) exits: Vec<OnionExitDescriptor>,
}

impl OnionRouteCandidates {
    /// Collect the live registrants of `relay` and of `service` in the local DHT protocol mode.
    ///
    /// The directory may be a remote node, so its descriptors are re-validated here: signature,
    /// liveness, the local DHT protocol mode, and the newest descriptor per DID.
    pub(crate) fn from_validated_descriptors(
        local: Did,
        dht_protocol: DhtProtocolMode,
        now_ms: u128,
        service: &OnionServiceName,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        exits: impl IntoIterator<Item = OnionExitDescriptor>,
    ) -> Self {
        Self {
            relays: eligible_relays(dht_protocol, now_ms, local, online_nodes),
            exits: eligible_exits(dht_protocol.network_id, now_ms, service, exits)
                .into_iter()
                .filter(|descriptor| descriptor.did != local)
                .collect(),
        }
    }
}

/// Select a route for `request` from live candidates and an explicit guard policy.
///
/// ```text
/// E  = exits of the service                        E = ∅                     → NoLiveExit
/// E′ = { e ∈ E | hop(e) ∈ R }   (D2)               E′ = ∅, some DID(e) ∈ R   → ExitRelayRegistrationMismatch
///                                                  E′ = ∅ otherwise           → ExitWithoutRelayRegistration
/// loop ← select_loop(ε ⋙ E, R)                     its symbol position drawn from E′
/// ```
///
/// Callers must state the guard policy explicitly, so a permissive default cannot bypass
/// entry-guard policy.
pub(crate) fn select_onion_route_from_candidates(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let service = request.service_name();
    let OnionRouteCandidates { relays, exits } = candidates;
    if !exits
        .iter()
        .any(|exit| relays.contains(&OnionRouteHop::of_symbol(exit)))
    {
        let service = service.as_str().to_string();
        let error = if exits.is_empty() {
            OnionRouteError::NoLiveExit { service }
        } else if exits
            .iter()
            .any(|exit| relays.iter().any(|relay| relay.did == exit.did))
        {
            OnionRouteError::ExitRelayRegistrationMismatch { service }
        } else {
            OnionRouteError::ExitWithoutRelayRegistration { service }
        };
        return Err(Error::OnionRouteError(error));
    }
    let drawn = select_loop(
        OnionPipelineSymbols::new(&[], &exits),
        OnionRouteHop::of_symbol,
        relays.as_slice(),
        &qualities.into_iter().collect(),
        entropy,
        guard_permitted,
    )?;
    let (hops, (_, exit)) = drawn.project_symbols(|(hop, _)| *hop);
    OnionRoute::new(service.clone(), hops, exit)
}

/// A symbol registrant drawable at a symbol position: its relay hop, and the registration `X`.
type SymbolCandidate<X> = (OnionRouteHop, X);

/// State threaded through one loop draw: the hops already taken and the entropy source.
struct LoopDraw<'e, E> {
    /// DIDs of the positions drawn so far.
    taken: BTreeSet<Did>,
    /// Source of the weighted draws.
    entropy: &'e mut E,
}

/// Draw a loop for the pipeline `symbols`, each symbol given by the registrations `X` of its
/// registrants, projected to their hops by `project`.
///
/// Every hop comes from `relays`, the relay registrants: a registration whose hop is not in
/// `relays` registers no `relay` at its epoch and is dropped at entry (D2), so every candidate
/// Hall's condition counts is drawable. The loop is unfolded position by position
/// ([`OnionLoop::try_unfold`]); at each position, with `T` the hops already taken and `Sₚ` the
/// candidates of the symbols still pending after it, one hop is drawn by quality weight, in DID
/// order, from
///
/// ```text
/// Guard       { r ∈ R ∖ T | guard_permitted(r) ∧ SDR(Sₚ, T ∪ {r}) }   none → NoPermittedFirstHop
///                                                                          or NoDistinctSymbolHops
/// Relay       { r ∈ R ∖ T | SDR(Sₚ, T ∪ {r}) }                        none → NotEnoughLoopHops
/// Symbol f    { c ∈ f ∖ T | SDR(Sₚ, T ∪ {c}) }                        none → NoDistinctSymbolHops
/// ```
///
/// where `SDR(S, T)` is Hall's condition that the pending symbols can still take pairwise
/// distinct hops outside `T`. Every draw keeps `SDR(pending, T)`, so a draw never strands a later
/// symbol: a relay draw fails exactly when fewer than `H − 1` distinct relays exist (fail closed,
/// D5), and a symbol draw cannot fail once the guard is drawn.
fn select_loop<X: Clone>(
    symbols: OnionPipelineSymbols<'_, Vec<X>>,
    project: impl Fn(&X) -> OnionRouteHop,
    relays: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionLoop<OnionRouteHop, SymbolCandidate<X>>> {
    let mut relays = relays.to_vec();
    relays.sort_by_key(|hop| hop.did);
    let candidates = |registrations: &Vec<X>| {
        let mut candidates = registrations
            .iter()
            .map(|registration| (project(registration), registration.clone()))
            .filter(|(hop, _)| relays.contains(hop))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(hop, _)| hop.did);
        candidates
    };
    let intermediate = symbols
        .intermediate()
        .iter()
        .map(candidates)
        .collect::<Vec<_>>();
    let terminal = candidates(symbols.terminal());
    OnionLoop::try_unfold(
        OnionPipelineSymbols::new(intermediate.as_slice(), &terminal),
        &mut LoopDraw {
            taken: BTreeSet::new(),
            entropy,
        },
        |draw, relay, cursor| {
            let pending = pending_dids(&cursor.pending);
            let hop = draw_weighted(
                relays.iter().copied().filter(|hop| {
                    (relay == OnionLoopRelay::Relay || guard_permitted(hop.did))
                        && !draw.taken.contains(&hop.did)
                        && admits_distinct_symbol_hops(pending.as_slice(), |did| {
                            *did == hop.did || draw.taken.contains(did)
                        })
                }),
                |hop| hop.did,
                quality_by_did,
                draw.entropy,
            )
            .ok_or_else(|| {
                Error::OnionRouteError(match relay {
                    OnionLoopRelay::Relay => OnionRouteError::NotEnoughLoopHops {
                        required: cursor.shape.distinct_hops(),
                        eligible: relays.len(),
                    },
                    OnionLoopRelay::Guard
                        if admits_distinct_symbol_hops(pending.as_slice(), |_| false) =>
                    {
                        OnionRouteError::NoPermittedFirstHop
                    }
                    OnionLoopRelay::Guard => OnionRouteError::NoDistinctSymbolHops,
                })
            })?;
            draw.taken.insert(hop.did);
            Ok(hop)
        },
        |draw, symbol, cursor| {
            let pending = pending_dids(&cursor.pending);
            let candidate = draw_weighted(
                symbol
                    .iter()
                    .filter(|(hop, _)| {
                        !draw.taken.contains(&hop.did)
                            && admits_distinct_symbol_hops(pending.as_slice(), |did| {
                                *did == hop.did || draw.taken.contains(did)
                            })
                    })
                    .cloned(),
                |(hop, _)| hop.did,
                quality_by_did,
                draw.entropy,
            )
            .ok_or(Error::OnionRouteError(
                OnionRouteError::NoDistinctSymbolHops,
            ))?;
            draw.taken.insert(candidate.0.did);
            Ok(candidate)
        },
    )
}

/// Return the candidate DIDs of each pending symbol, in pipeline order.
fn pending_dids<X>(pending: &OnionPending<'_, Vec<SymbolCandidate<X>>>) -> Vec<Vec<Did>> {
    pending
        .iter()
        .map(|candidates| candidates.iter().map(|(hop, _)| hop.did).collect())
        .collect()
}

/// Hall's condition for the pending symbol positions,
///
/// ```text
/// SDR(S, T)  ⇔  ∀ J ⊆ S.  |⋃J ∖ T| ≥ |J|,
/// ```
///
/// decided by its equivalent: a matching that gives every symbol of `symbols` its own DID outside
/// the `excluded` set exists. Each symbol is matched in turn along an augmenting path (Kuhn), so
/// the check takes `O(n · Σ|Sᵢ|)` steps and builds no subfamily.
fn admits_distinct_symbol_hops(symbols: &[Vec<Did>], excluded: impl Fn(&Did) -> bool) -> bool {
    let mut owner = BTreeMap::new();
    (0..symbols.len()).all(|symbol| {
        augment_symbol_matching(symbols, symbol, &excluded, &mut owner, &mut BTreeSet::new())
    })
}

/// Extend the matching `owner` (DID ↦ symbol index) to cover `symbol`, re-matching symbols along
/// an augmenting path; `visited` holds the DIDs this search has tried.
fn augment_symbol_matching(
    symbols: &[Vec<Did>],
    symbol: usize,
    excluded: &impl Fn(&Did) -> bool,
    owner: &mut BTreeMap<Did, usize>,
    visited: &mut BTreeSet<Did>,
) -> bool {
    symbols.get(symbol).is_some_and(|dids| {
        dids.iter().any(|did| {
            if excluded(did) || !visited.insert(*did) {
                return false;
            }
            let free = match owner.get(did).copied() {
                None => true,
                Some(other) => augment_symbol_matching(symbols, other, excluded, owner, visited),
            };
            if free {
                owner.insert(*did, symbol);
            }
            free
        })
    })
}

/// Draw one candidate with probability proportional to the quality weight of its DID, or `None`
/// when there is no candidate.
fn draw_weighted<T>(
    candidates: impl Iterator<Item = T>,
    did_of: impl Fn(&T) -> Did,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<T> {
    let candidates = candidates.collect::<Vec<_>>();
    let dids = candidates.iter().map(&did_of).collect::<Vec<_>>();
    let index = pick_weighted_index(dids.as_slice(), quality_by_did, entropy)?;
    candidates.into_iter().nth(index)
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
/// protocol mode registers `relay`, one per DID, at the epoch it registers it with.
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
