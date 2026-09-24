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
//! - **Draw order** (L7). The symbol positions are drawn first, then the guard, then the relays,
//!   so when every registrant of the first symbol `h₁` extends to a loop, its marginal is its
//!   quality share among them; for a one-symbol pipeline `h₁` is the exit.
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
/// E  = exits of the service              E = ∅                    → NoLiveExit
/// E′ = { e ∈ E | hop(e) ∈ R }   (D2)     E′ = ∅, some DID(e) ∈ R  → ExitRelayRegistrationMismatch
///                                        E′ = ∅ otherwise         → ExitWithoutRelayRegistration
/// loop ← select_loop(ε ⋙ E′, R)
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
    let relays = RelayRegistrants::new(relays);
    let admitted = relays.admit(exits.iter(), |exit| OnionRouteHop::of_symbol(exit));
    if admitted.is_empty() {
        let service = service.as_str().to_string();
        let error = if exits.is_empty() {
            OnionRouteError::NoLiveExit { service }
        } else if exits.iter().any(|exit| relays.get(exit.did).is_some()) {
            OnionRouteError::ExitRelayRegistrationMismatch { service }
        } else {
            OnionRouteError::ExitWithoutRelayRegistration { service }
        };
        return Err(Error::OnionRouteError(error));
    }
    let drawn = select_loop(
        OnionPipelineSymbols::single(&admitted),
        &relays,
        &qualities.into_iter().collect(),
        entropy,
        guard_permitted,
    )?;
    let (hops, (_, exit)) = drawn.project_symbols(|(hop, _)| *hop);
    OnionRoute::new(service.clone(), hops, exit.clone())
}

/// The relay registrants of one draw: one hop per DID, in DID order.
struct RelayRegistrants(Vec<OnionRouteHop>);

impl RelayRegistrants {
    /// Order `relays` by DID, keeping one hop per DID.
    fn new(mut relays: Vec<OnionRouteHop>) -> Self {
        relays.sort_by_key(|hop| hop.did);
        relays.dedup_by_key(|hop| hop.did);
        Self(relays)
    }

    /// Return the relay registration of `did`, if any.
    fn get(&self, did: Did) -> Option<&OnionRouteHop> {
        self.0
            .binary_search_by_key(&did, |hop| hop.did)
            .ok()
            .and_then(|index| self.0.get(index))
    }

    /// Admit the registrations of one symbol whose hop is a relay registration of this draw
    /// (D2), in DID order and keeping the first registration of each DID: the only way to build
    /// a [`SymbolRegistrants`].
    fn admit<X>(
        &self,
        registrations: impl IntoIterator<Item = X>,
        project: impl Fn(&X) -> OnionRouteHop,
    ) -> SymbolRegistrants<X> {
        let mut admitted = registrations
            .into_iter()
            .map(|registration| (project(&registration), registration))
            .filter(|(hop, _)| self.get(hop.did) == Some(hop))
            .collect::<Vec<_>>();
        admitted.sort_by_key(|(hop, _)| hop.did);
        admitted.dedup_by_key(|(hop, _)| hop.did);
        SymbolRegistrants {
            dids: admitted.iter().map(|(hop, _)| hop.did).collect(),
            registrations: admitted,
        }
    }
}

/// The registrations of one symbol whose hops are relay registrations of the draw (D2), in DID
/// order and one per DID. Every hop Hall's condition counts is therefore drawable (#846 N1), and
/// each registrant's draw weight is its quality weight once.
///
/// Invariant: `dids = map (did ∘ fst) registrations`, projected once by [`RelayRegistrants::admit`]
/// so every matching borrows it.
struct SymbolRegistrants<X> {
    /// The admitted registrations with their hops, in DID order.
    registrations: Vec<(OnionRouteHop, X)>,
    /// The DIDs of `registrations`, in the same order.
    dids: Vec<Did>,
}

impl<X> SymbolRegistrants<X> {
    /// Return whether no registration was admitted.
    fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// Return the DIDs of the admitted registrations, in DID order.
    fn dids(&self) -> &[Did] {
        self.dids.as_slice()
    }
}

/// Draw a loop for the pipeline `symbols` over the relay registrants `relays` (#834 L7).
///
/// The draw order is not the position order. Each draw is weighted by quality in DID order, and
/// keeps a matching of every position still pending: the later symbols and, until it is drawn,
/// the guard, whose candidates `G = guard_permitted ∩ R` are the only source of its draw.
///
/// ```text
/// shape ← OnionLoopShape::new(n)                     n ∉ [1, 4]         → LoopSymbolsOutOfBounds
/// |R| < H − 1                                                            → NotEnoughLoopHops  (D5)
/// no matching of (S₁ … Sₙ)                                               → NoDistinctSymbolHops
/// no matching of (S₁ … Sₙ, G)                                            → NoPermittedFirstHop
/// hₖ ← draw { c ∈ Sₖ ∖ T | matching(Sₖ₊₁ … Sₙ, G; T ∪ {c}) }           for k = 1 … n
/// g  ← draw G ∖ T
/// the H − 2 − n relays ← draws from R ∖ T, placed by OnionLoop::try_unfold
/// ```
///
/// When every registrant of the first symbol extends to a loop (the matching admits it), drawing
/// the symbols first makes each registrant's marginal for `h₁` its quality share among them, so a
/// scarce high-quality exit of a one-symbol pipeline is not consumed as the guard or a relay
/// first. The later symbols are drawn conditionally on `T`, so their marginals are not claimed. A
/// registrant the matching excludes, e.g. the only permitted guard, is never drawn for the symbol.
///
/// No draw is empty once the checks above pass. With `T` the DIDs taken so far:
///
/// ```text
/// P1  Sₖ ⊆ R and G ⊆ R, one hop per DID          (RelayRegistrants::admit, D2)
/// P2  matching(S₁ … Sₙ, G; ∅)                     (the NoPermittedFirstHop check)
/// P3  |R| ≥ H − 1                                  (the NotEnoughLoopHops check, D5)
///
/// I   matching(Sₖ … Sₙ, G; T) before draw k       (k = 1 by P2)
///     ⇒ its Sₖ-element c ∉ T witnesses matching(Sₖ₊₁ … Sₙ, G; T ∪ {c}),
///       so the filtered draw of hₖ is non-empty and I holds for k + 1
/// g   I at k = n + 1 is matching(G; T) ⇒ G ∖ T ≠ ∅
/// r   |T| = n + 1 and T ⊆ R (P1) ⇒ |R ∖ T| ≥ H − 2 − n (P3): every relay draw is non-empty
/// ```
///
/// An empty draw is therefore reported as `LoopDrawInvariant`, a violated invariant, never as a
/// property of the network.
fn select_loop<'x, X>(
    symbols: OnionPipelineSymbols<'_, SymbolRegistrants<&'x X>>,
    relays: &RelayRegistrants,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionLoop<OnionRouteHop, (OnionRouteHop, &'x X)>> {
    let shape = OnionLoopShape::new(symbols.symbol_count())?;
    if relays.0.len() < shape.distinct_hops() {
        return Err(Error::OnionRouteError(OnionRouteError::NotEnoughLoopHops {
            required: shape.distinct_hops(),
            eligible: relays.0.len(),
        }));
    }
    let guards = relays
        .0
        .iter()
        .filter(|hop| guard_permitted(hop.did))
        .collect::<Vec<_>>();
    let guard_dids = guards.iter().map(|hop| hop.did).collect::<Vec<_>>();
    if !admits_distinct_hops(PendingPositions::symbols(symbols.as_slice()), |_| false) {
        return Err(Error::OnionRouteError(
            OnionRouteError::NoDistinctSymbolHops,
        ));
    }
    if !admits_distinct_hops(
        PendingPositions::new(symbols.as_slice(), &guard_dids),
        |_| false,
    ) {
        return Err(Error::OnionRouteError(OnionRouteError::NoPermittedFirstHop));
    }

    let invariant = || Error::OnionRouteError(OnionRouteError::LoopDrawInvariant);
    let mut taken = BTreeSet::new();
    let (intermediate, terminal) = symbols.try_map_with_later(|registrants, later| {
        let pending = PendingPositions::new(later, &guard_dids);
        let drawn = *draw_weighted(
            registrants.registrations.iter().filter(|(hop, _)| {
                !taken.contains(&hop.did)
                    && admits_distinct_hops(pending, |did| *did == hop.did || taken.contains(did))
            }),
            |(hop, _)| hop.did,
            quality_by_did,
            entropy,
        )
        .ok_or_else(invariant)?;
        taken.insert(drawn.0.did);
        Ok(drawn)
    })?;
    let guard = **draw_weighted(
        guards.iter().filter(|hop| !taken.contains(&hop.did)),
        |hop| hop.did,
        quality_by_did,
        entropy,
    )
    .ok_or_else(invariant)?;
    taken.insert(guard.did);

    OnionLoop::try_unfold(intermediate, terminal, |relay| match relay {
        OnionLoopRelay::Guard => Ok(guard),
        OnionLoopRelay::Relay => {
            let hop = *draw_weighted(
                relays.0.iter().filter(|hop| !taken.contains(&hop.did)),
                |hop| hop.did,
                quality_by_did,
                entropy,
            )
            .ok_or_else(invariant)?;
            taken.insert(hop.did);
            Ok(hop)
        }
    })
}

/// The positions still to be matched: the pending symbols, then the guard until it is drawn.
struct PendingPositions<'p, X> {
    /// The registrants of each pending symbol, in pipeline order.
    symbols: &'p [SymbolRegistrants<X>],
    /// Candidate DIDs of the guard, while the guard is pending.
    guard: Option<&'p [Did]>,
}

/// A view of borrowed slices: copyable for every `X`, which `#[derive(Copy)]` would bound.
impl<X> Clone for PendingPositions<'_, X> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<X> Copy for PendingPositions<'_, X> {}

impl<'p, X> PendingPositions<'p, X> {
    /// The pending `symbols` followed by the guard's candidates `guard`.
    const fn new(symbols: &'p [SymbolRegistrants<X>], guard: &'p [Did]) -> Self {
        Self {
            symbols,
            guard: Some(guard),
        }
    }

    /// The pending `symbols` alone.
    const fn symbols(symbols: &'p [SymbolRegistrants<X>]) -> Self {
        Self {
            symbols,
            guard: None,
        }
    }

    /// Return the number of pending positions.
    fn len(self) -> usize {
        self.symbols.len() + usize::from(self.guard.is_some())
    }

    /// Return the candidates of position `index`: a symbol, or the guard after the symbols.
    fn get(self, index: usize) -> Option<&'p [Did]> {
        match self.symbols.get(index) {
            Some(registrants) => Some(registrants.dids()),
            None => self.guard.filter(|_| index == self.symbols.len()),
        }
    }
}

/// Decide whether the `pending` positions can take pairwise distinct DIDs outside `excluded`:
/// Hall's condition for their candidate families,
///
/// ```text
/// SDR(P, T)  ⇔  ∀ J ⊆ P.  |⋃J ∖ T| ≥ |J|,
/// ```
///
/// decided by its equivalent: a matching that gives every position its own DID. Each position
/// is matched in turn along an augmenting path (Kuhn), in `O(|P| · Σ|Pᵢ|)` steps.
fn admits_distinct_hops<X>(
    pending: PendingPositions<'_, X>,
    excluded: impl Fn(&Did) -> bool,
) -> bool {
    let mut owner = BTreeMap::new();
    (0..pending.len()).all(|position| {
        augment_matching(
            pending,
            position,
            &excluded,
            &mut owner,
            &mut BTreeSet::new(),
        )
    })
}

/// Extend the matching `owner` (DID ↦ position index) to cover `position`, re-matching positions
/// along an augmenting path; `visited` holds the DIDs this search has tried.
fn augment_matching<X>(
    pending: PendingPositions<'_, X>,
    position: usize,
    excluded: &impl Fn(&Did) -> bool,
    owner: &mut BTreeMap<Did, usize>,
    visited: &mut BTreeSet<Did>,
) -> bool {
    pending.get(position).is_some_and(|dids| {
        dids.iter().any(|did| {
            if excluded(did) || !visited.insert(*did) {
                return false;
            }
            let free = match owner.get(did).copied() {
                None => true,
                Some(other) => augment_matching(pending, other, excluded, owner, visited),
            };
            if free {
                owner.insert(*did, position);
            }
            free
        })
    })
}

/// Draw one candidate with probability proportional to the quality weight of its DID, or `None`
/// when there is no candidate. The draw is over references; the caller copies the winner.
fn draw_weighted<'c, T>(
    candidates: impl Iterator<Item = &'c T>,
    did_of: impl Fn(&T) -> Did,
    quality_by_did: &BTreeMap<Did, PeerQuality>,
    entropy: &mut impl RouteEntropy,
) -> Option<&'c T> {
    let candidates = candidates.collect::<Vec<_>>();
    let dids = candidates
        .iter()
        .map(|candidate| did_of(candidate))
        .collect::<Vec<_>>();
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
