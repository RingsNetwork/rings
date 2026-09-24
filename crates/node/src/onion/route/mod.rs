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

use rings_core::dht::Did;
use rings_core::ecc::PublicKey;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::pipeline::OnionPipeline;
use super::pipeline::OnionSymbolWord;
use super::OnionExitDescriptor;
use super::OnionLoop;
use super::OnionLoopShape;
use super::OnionLoopStep;
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
            .prefix_to_symbol(OnionLoopShape::SESSION.symbols())
    }

    /// Return the Phase 1 view of [`Self::circuit_hops`]: the relays before `h₁`, then the exit.
    pub(crate) fn positions(&self) -> OnionPipeline<OnionRouteHop> {
        OnionPipeline::new(
            self.circuit_hops()
                .take_while(|hop| hop.did != self.exit.did)
                .copied()
                .collect(),
            OnionRouteHop::of_symbol(&self.exit),
        )
    }

    /// Return the symbol word `relay^k ⋙ service` of [`Self::circuit_hops`].
    pub fn word(&self) -> OnionSymbolWord {
        OnionSymbolWord::new(
            self.circuit_hops().count().saturating_sub(1),
            self.service.clone(),
        )
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
/// E  = exits of the service                       E = ∅                      → NoLiveExit
/// E′ = { e ∈ E | hop(e) ∈ R }   (D2)              E′ = ∅, some DID(e) ∈ R    → StaleExitRegistration
///                                                 E′ = ∅ otherwise            → ExitWithoutRelayRegistration
/// e  ← draw { e ∈ E′ | ∃ r ∈ R. r ≠ e ∧ guard_permitted(r) }         none → NoPermittedFirstHop
/// loop ← select_loop([{e}], R)
/// ```
///
/// The exit is drawn first, among the exits some permitted guard can precede, so the loop drawn
/// around it never lacks a guard. Callers must state the guard policy explicitly, so a permissive
/// default cannot bypass entry-guard policy.
pub(crate) fn select_onion_route_from_candidates(
    request: &OnionRouteRequest,
    candidates: OnionRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionRoute> {
    let service = request.service_name();
    let OnionRouteCandidates { relays, exits } = candidates;
    let (registered, unregistered) = exits
        .into_iter()
        .partition::<Vec<_>, _>(|exit| relays.contains(&OnionRouteHop::of_symbol(exit)));
    if registered.is_empty() {
        let service = service.as_str().to_string();
        let error = if unregistered.is_empty() {
            OnionRouteError::NoLiveExit { service }
        } else if unregistered
            .iter()
            .any(|exit| relays.iter().any(|relay| relay.did == exit.did))
        {
            OnionRouteError::StaleExitRegistration { service }
        } else {
            OnionRouteError::ExitWithoutRelayRegistration { service }
        };
        return Err(Error::OnionRouteError(error));
    }
    let quality_by_did = qualities.into_iter().collect::<BTreeMap<_, _>>();
    let exit = draw_weighted(
        registered.into_iter().filter(|exit| {
            relays
                .iter()
                .any(|relay| relay.did != exit.did && guard_permitted(relay.did))
        }),
        |exit| exit.did,
        &quality_by_did,
        entropy,
    )
    .ok_or(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop))?;
    let hops = select_loop(
        &[BTreeSet::from([exit.did])],
        relays.as_slice(),
        &quality_by_did,
        entropy,
        guard_permitted,
    )?;
    OnionRoute::new(service.clone(), hops, exit)
}

/// Draw a loop for the pipeline whose `k`-th symbol is registered by `symbols[k − 1]`.
///
/// Every hop comes from `relays`, the relay registrants, so a symbol registrant outside `relays`
/// registers no `relay` and is never drawn (D2). The loop is unfolded position by position
/// ([`OnionLoop::try_unfold`]); at each position, with `T` the hops already taken and `Sₚ` the
/// symbols still pending after it, one hop is drawn by quality weight from
///
/// ```text
/// Guard       { r ∈ R ∖ T | guard_permitted(r) ∧ SDR(Sₚ, T ∪ {r}) }   none → NoPermittedFirstHop
///                                                                          or NoDistinctSymbolHops
/// Relay       { r ∈ R ∖ T | SDR(Sₚ, T ∪ {r}) }                        none → NotEnoughLoopHops
/// Symbol f    { r ∈ R ∖ T | r ∈ f ∧ SDR(Sₚ, T ∪ {r}) }               none → NoDistinctSymbolHops
/// ```
///
/// where `SDR(S, T)` is Hall's condition that the pending symbols can still take pairwise
/// distinct hops outside `T`. Every draw keeps `SDR(pending, T)`, so a draw never strands a later
/// symbol: a relay draw fails exactly when fewer than `H − 1` distinct relays exist (fail closed,
/// D5), and a symbol draw cannot fail once the guard is drawn.
fn select_loop(
    symbols: &[BTreeSet<Did>],
    relays: &[OnionRouteHop],
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionLoop<OnionRouteHop>> {
    let required = OnionLoopShape::new(symbols.len())?.distinct_hops();
    let mut taken = BTreeSet::new();
    OnionLoop::try_unfold(symbols, |step| {
        let pending = step.pending();
        let hop = draw_weighted(
            relays.iter().copied().filter(|hop| {
                let eligible = match &step {
                    OnionLoopStep::Guard { .. } => guard_permitted(hop.did),
                    OnionLoopStep::Relay { .. } => true,
                    OnionLoopStep::Symbol { symbol, .. } => symbol.contains(&hop.did),
                };
                eligible
                    && !taken.contains(&hop.did)
                    && admits_distinct_symbol_hops(pending, |did| {
                        *did == hop.did || taken.contains(did)
                    })
            }),
            |hop| hop.did,
            quality_by_did,
            entropy,
        )
        .ok_or_else(|| {
            Error::OnionRouteError(match step {
                OnionLoopStep::Guard { .. } if admits_distinct_symbol_hops(symbols, |_| false) => {
                    OnionRouteError::NoPermittedFirstHop
                }
                OnionLoopStep::Relay { .. } => OnionRouteError::NotEnoughLoopHops {
                    required,
                    eligible: relays.len(),
                },
                OnionLoopStep::Guard { .. } | OnionLoopStep::Symbol { .. } => {
                    OnionRouteError::NoDistinctSymbolHops
                }
            })
        })?;
        taken.insert(hop.did);
        Ok(hop)
    })
}

/// Hall's condition for the pending symbol positions: every subfamily `J` of `symbols`, less the
/// `excluded` hops, covers at least `|J|` hops,
///
/// ```text
/// SDR(S, T)  ⇔  ∀ J ⊆ S.  |⋃J ∖ T| ≥ |J|,
/// ```
///
/// which holds exactly when the positions can still take pairwise distinct hops outside `T`. The
/// subfamilies are folded one symbol at a time, each paired with its size and union, so the
/// check needs no index arithmetic; a loop has at most `2^n_max = 16` of them.
fn admits_distinct_symbol_hops(symbols: &[BTreeSet<Did>], excluded: impl Fn(&Did) -> bool) -> bool {
    symbols
        .iter()
        .fold(
            vec![(0_usize, BTreeSet::<&Did>::new())],
            |families, registrant| {
                let extended = families
                    .iter()
                    .map(|(size, union)| {
                        let mut union = union.clone();
                        union.extend(registrant.iter().filter(|did| !excluded(did)));
                        (size + 1, union)
                    })
                    .collect::<Vec<_>>();
                families.into_iter().chain(extended).collect()
            },
        )
        .iter()
        .all(|(size, union)| union.len() >= *size)
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
