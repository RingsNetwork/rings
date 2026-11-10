//! Multi-stage route selection for intermediate `invoke` Kleisli pipelines (#834 D2, D4, D5, L7).
//!
//! Places a non-empty sequence `(f₁, ω₁) … (fₙ, ωₙ)` (`1 ≤ n ≤ MAX_ONION_LOOP_SYMBOLS`) onto a
//! guard-closed loop of `H = (s + 1)n + s` positions:
//!
//! - every symbol position `hₖ` registers `(fₖ, ωₖ)` under [`crate::onion::ONION_INVOKES_TOPIC`]
//!   at the same DID, session key, and process epoch `e_n` as its `relay` registration (D2);
//! - symbol positions `h₁ … hₙ` are drawn first, Hall-filtered against pending symbols and the
//!   guard, followed by the guard `g` and the remaining relays (L7);
//! - selection fails closed below `H − 1` distinct eligible relay registrants (D5).

use rings_core::dht::Did;
use rings_core::measure::PeerQuality;
use rings_core::message::DhtProtocolMode;

use super::eligible_relays;
use super::select_loop;
use super::OnionRouteHop;
use super::RelayRegistrants;
use super::RouteEntropy;
use crate::error::Error;
use crate::error::Result;
use crate::onion::invoke_descriptor::OnionInvokeDescriptor;
use crate::onion::loop_shape::OnionPipelineSymbols;
use crate::onion::pipeline::OnionInvokePipeline;
use crate::onion::signature::OnionBackendName;
use crate::onion::signature::OnionInvokeShape;
use crate::onion::OnionLoop;
use crate::onion::OnionRouteError;
use crate::onion::MAX_ONION_LOOP_SYMBOLS;
use crate::online::OnlineNodeDescriptor;

/// Route-building request for a 1..=4 stage `invoke` Kleisli pipeline (#834 D4a, Phase 2b).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionPipelineRouteRequest {
    symbols: Vec<(OnionInvokeShape, OnionBackendName)>,
}

impl OnionPipelineRouteRequest {
    /// Build a route request from `(shape, backend)` pairs, rejecting `n = 0` or `n > 4`.
    pub fn new(
        symbols: impl IntoIterator<Item = (OnionInvokeShape, OnionBackendName)>,
    ) -> Result<Self> {
        let symbols = symbols.into_iter().collect::<Vec<_>>();
        if symbols.is_empty() || symbols.len() > MAX_ONION_LOOP_SYMBOLS {
            return Err(Error::OnionRouteError(
                OnionRouteError::LoopSymbolsOutOfBounds {
                    symbols: symbols.len(),
                    max_symbols: MAX_ONION_LOOP_SYMBOLS,
                },
            ));
        }
        Ok(Self { symbols })
    }

    /// Build the route request required by `pipeline` (omitting inline client constants `ā′`).
    pub fn from_pipeline(pipeline: &OnionInvokePipeline) -> Self {
        Self {
            symbols: pipeline.required_symbols(),
        }
    }

    /// Return the requested `(shape, backend)` pairs in stage order.
    pub fn symbols(&self) -> &[(OnionInvokeShape, OnionBackendName)] {
        self.symbols.as_slice()
    }
}

/// Selected guard-closed loop route for an `n`-stage `invoke` Kleisli pipeline (#834 D4c, D5, L7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnionPipelineRoute {
    symbols: Vec<(OnionInvokeShape, OnionBackendName)>,
    hops: OnionLoop<OnionRouteHop>,
    descriptors: Vec<OnionInvokeDescriptor>,
}

impl OnionPipelineRoute {
    /// Construct a pipeline route over `hops` and the `n` stage `descriptors`.
    pub(crate) fn new(
        symbols: Vec<(OnionInvokeShape, OnionBackendName)>,
        hops: OnionLoop<OnionRouteHop>,
        descriptors: Vec<OnionInvokeDescriptor>,
    ) -> Self {
        Self {
            symbols,
            hops,
            descriptors,
        }
    }

    /// Return the `(shape, backend)` pairs selected along the pipeline.
    pub fn symbols(&self) -> &[(OnionInvokeShape, OnionBackendName)] {
        self.symbols.as_slice()
    }

    /// Return the guard-closed loop hop assignment (`H = (s + 1)n + s` positions).
    pub const fn hops(&self) -> &OnionLoop<OnionRouteHop> {
        &self.hops
    }

    /// Return the signed `invoke` descriptors selected for `h₁ … hₙ`, in pipeline order.
    pub fn descriptors(&self) -> &[OnionInvokeDescriptor] {
        self.descriptors.as_slice()
    }

    /// Return whether this route's `(shape, backend)` sequence matches `pipeline`'s stages.
    pub fn matches_pipeline(&self, pipeline: &OnionInvokePipeline) -> bool {
        self.symbols.len() == pipeline.len()
            && self
                .symbols
                .iter()
                .zip(pipeline.stages().iter())
                .all(|((shape, backend), stage)| {
                    *shape == stage.shape() && backend == stage.backend()
                })
    }
}

/// Live registrants for multi-stage `invoke` route selection.
#[derive(Clone, Debug)]
pub(crate) struct OnionPipelineRouteCandidates {
    pub(in crate::onion) relays: Vec<OnionRouteHop>,
    pub(in crate::onion) invokes: Vec<OnionInvokeDescriptor>,
}

impl OnionPipelineRouteCandidates {
    /// Re-validate remote `online_nodes` and `invokes` against signatures, liveness, local
    /// `dht_protocol`, and `did != local`.
    pub(crate) fn from_validated_descriptors(
        local: Did,
        dht_protocol: DhtProtocolMode,
        now_ms: u128,
        online_nodes: impl IntoIterator<Item = OnlineNodeDescriptor>,
        invokes: impl IntoIterator<Item = OnionInvokeDescriptor>,
    ) -> Self {
        let invokes = OnionInvokeDescriptor::latest_valid_by_symbol_did(
            invokes,
            now_ms,
            dht_protocol.network_id,
            false,
        )
        .into_iter()
        .filter(|descriptor| descriptor.did != local)
        .collect();
        Self {
            relays: eligible_relays(dht_protocol, now_ms, local, online_nodes),
            invokes,
        }
    }
}

/// Select a guard-closed loop route for `request` from `candidates` and an explicit guard policy.
pub(crate) fn select_onion_pipeline_route_from_candidates(
    request: &OnionPipelineRouteRequest,
    candidates: OnionPipelineRouteCandidates,
    qualities: impl IntoIterator<Item = (Did, PeerQuality)>,
    entropy: &mut impl RouteEntropy,
    guard_permitted: impl Fn(Did) -> bool,
) -> Result<OnionPipelineRoute> {
    let OnionPipelineRouteCandidates { relays, invokes } = candidates;
    let relays = RelayRegistrants::new(relays);
    let mut stage_registrants = Vec::with_capacity(request.symbols().len());
    for (shape, backend) in request.symbols() {
        let matching = invokes
            .iter()
            .filter(|desc| desc.shape == *shape && desc.backend == *backend)
            .collect::<Vec<_>>();
        let admitted = relays.admit(matching.iter().copied(), |desc| {
            OnionRouteHop::new(desc.did, desc.delegatee_public_key, desc.process_epoch)
        });
        if admitted.is_empty() {
            let shape_str = shape.to_string();
            let backend_str = backend.as_str().to_string();
            let error = if matching.is_empty() {
                OnionRouteError::NoLiveSymbol {
                    shape: shape_str,
                    backend: backend_str,
                }
            } else if matching.iter().any(|desc| relays.get(desc.did).is_some()) {
                OnionRouteError::SymbolRelayRegistrationMismatch {
                    shape: shape_str,
                    backend: backend_str,
                }
            } else {
                OnionRouteError::SymbolWithoutRelayRegistration {
                    shape: shape_str,
                    backend: backend_str,
                }
            };
            return Err(Error::OnionRouteError(error));
        }
        stage_registrants.push(admitted);
    }

    let pipeline_symbols = OnionPipelineSymbols::new(stage_registrants.as_slice()).ok_or(
        Error::OnionRouteError(OnionRouteError::LoopSymbolsOutOfBounds {
            symbols: 0,
            max_symbols: MAX_ONION_LOOP_SYMBOLS,
        }),
    )?;
    let drawn = select_loop(
        pipeline_symbols,
        &relays,
        &qualities.into_iter().collect(),
        entropy,
        guard_permitted,
    )?;
    let (hops, selected) = drawn.project_all_symbols(|(hop, _)| *hop);
    let descriptors = selected.into_iter().map(|(_, desc)| desc.clone()).collect();
    Ok(OnionPipelineRoute::new(
        request.symbols.clone(),
        hops,
        descriptors,
    ))
}
