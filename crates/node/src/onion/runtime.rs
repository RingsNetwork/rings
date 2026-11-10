//! The onion runtime of one node: its data plane installed in the protocol registry, and the
//! client its own sessions and Kleisli pipelines dial through. One construction for native and
//! browser nodes; the platform decides only which worlds the exit's algebra registers.

use std::num::NonZeroUsize;
use std::pin::pin;
use std::time::Duration;

use futures::future::select;
use futures::future::Either;
use rings_core::utils::get_epoch_ms;
use rings_runtime::sleep;

use super::circuit::OnionAlgebra;
use super::circuit::OnionCircuitProtocol;
use super::circuit::OnionCircuitShell;
use super::circuit::OnionClientTags;
use super::circuit::OnionInvokeBackend;
use super::circuit::OnionInvokeRegistry;
use super::circuit::OnionLinkFact;
use super::circuit::OnionLinkSender;
#[cfg(all(test, rings_native))]
use super::circuit::OnionLinkWitness;
use super::circuit::OnionLoopClient;
use super::circuit::ONION_CIRCUIT_NAMESPACE;
use super::exit_accounting::OnionExitAccounting;
use super::https::OnionHttpsWorld;
use super::invoke_descriptor::OnionInvokePolicy;
use super::pipeline::OnionInvokePipeline;
use super::route::OnionPipelineRoute;
use super::session::dial;
use super::session::dial::OnionClientStream;
use super::session::dial::OnionSessionRequest;
use super::session::serve::OnionExitSessions;
use super::signature::OnionBackendName;
use super::signature::OnionInvokeShape;
use super::sphinx::class::OnionLoopClass;
use super::OnionExitOffer;
use super::OnionRouteError;
use super::OnionServiceName;
use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Extensions;
use crate::extension::ext::Scope;

/// Installed onion runtime of one node.
#[derive(Clone)]
pub(crate) struct OnionRuntime {
    /// The data plane's scope, which the node's own loops are sent under.
    scope: Scope,
    /// The node's loop client.
    loops: OnionLoopClient,
    /// Shared `(shape, backend)` interpretation table for intermediate `invoke` evaluations.
    invokes: OnionInvokeRegistry,
    /// The witness of the data plane's link table.
    #[cfg(all(test, rings_native))]
    links: OnionLinkWitness,
    /// The exit services registered in the installed shell's algebra.
    #[cfg(all(test, rings_native))]
    registered_services: std::collections::BTreeSet<OnionServiceName>,
}

impl OnionRuntime {
    /// Install the data plane of the processor behind `extensions`.
    pub(crate) fn install(extensions: &Extensions) -> Result<Self> {
        let core = extensions.core();
        Self::install_with(extensions, |accounting, link_sender| {
            exit_algebra(core.onion_role().exit(), accounting, link_sender)
        })
    }

    /// [`Self::install`] with a custom world-facing `algebra` builder; the node's shared
    /// [`OnionInvokeRegistry`] is attached automatically so intermediate `invoke` backends can be
    /// registered on the runtime.
    pub(crate) fn install_with(
        extensions: &Extensions,
        algebra: impl FnOnce(&OnionExitAccounting, &OnionLinkSender) -> OnionAlgebra,
    ) -> Result<Self> {
        let core = extensions.core();
        let link_sender = OnionLinkSender::new(core.onion_idle_floor());
        let tags = OnionClientTags::default();
        let accounting = OnionExitAccounting::default();
        let invokes = OnionInvokeRegistry::new(core.onion_role().invoke(), link_sender.clone());
        let capacity = NonZeroUsize::new(core.connection_registry_capacity()?)
            .ok_or_else(|| Error::InvalidConfig("connection registry of capacity 0".to_string()))?;
        let built_algebra = algebra(&accounting, &link_sender).with_invokes(invokes.clone());
        let shell = OnionCircuitShell::new(
            core.onion_role().registers_relay(),
            core.onion_process_epoch(),
            capacity,
            core.slot_roster(),
            tags.clone(),
            link_sender.clone(),
            built_algebra,
        );
        #[cfg(all(test, rings_native))]
        let links = shell.link_witness();
        #[cfg(all(test, rings_native))]
        let registered_services = shell.registered_services();
        extensions.register(OnionCircuitProtocol, shell)?;
        let scope = Scope::new(core.clone(), ONION_CIRCUIT_NAMESPACE.to_string());
        // Registered first, so every admission after the read below reaches the data plane as a
        // fact behind it (`circuit::shell`, the linearisation obligation).
        let reconcile = OnionLinkFact::Reconcile.encode()?;
        let injecting = scope.clone();
        let tasks = injecting.tasks().clone();
        tasks.spawn(async move {
            if let Err(error) = injecting.inject(reconcile).await {
                tracing::warn!(%error, "onion data plane could not reconcile its link table");
            }
        })?;
        Ok(Self {
            scope,
            loops: OnionLoopClient::new(core.did(), tags, link_sender),
            invokes,
            #[cfg(all(test, rings_native))]
            links,
            #[cfg(all(test, rings_native))]
            registered_services,
        })
    }

    /// The witness of the data plane's link table.
    #[cfg(all(test, rings_native))]
    pub(crate) fn links(&self) -> &OnionLinkWitness {
        &self.links
    }

    /// Register an `invoke` backend implementation `⟦(shape, backend)⟧` on this node's Σ-algebra.
    pub(crate) fn register_invoke(
        &self,
        shape: OnionInvokeShape,
        backend: OnionBackendName,
        policy: OnionInvokePolicy,
        backend_impl: impl OnionInvokeBackend + 'static,
    ) -> Result<()> {
        self.invokes.register(shape, backend, policy, backend_impl)
    }

    /// Open one session over its selected route and wait for the exit's answer.
    pub(crate) async fn open(&self, request: OnionSessionRequest) -> Result<OnionClientStream> {
        dial::open(self.loops.clone(), self.scope.clone(), request).await
    }

    /// Evaluate a one-shot Kleisli `pipeline` over `route` at its L3-minimal loop class, waiting
    /// up to `timeout` (clamped by the loop's expiry `x`).
    pub(crate) async fn invoke_with_class_and_timeout(
        &self,
        route: &OnionPipelineRoute,
        pipeline: &OnionInvokePipeline,
        class: OnionLoopClass,
        input: &[u8],
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        let (receiver, tag, expiry) = self
            .loops
            .send_pipeline(&self.scope, route, pipeline, class, input)
            .await?;
        let remaining_expiry_ms =
            u64::try_from(expiry.as_ms().saturating_sub(get_epoch_ms())).unwrap_or(u64::MAX);
        let expiry_bound = Duration::from_millis(remaining_expiry_ms);
        let wait_bound = timeout.map_or(expiry_bound, |requested| requested.min(expiry_bound));
        match select(pin!(receiver), pin!(sleep(wait_bound))).await {
            Either::Left((Ok(output), _)) => Ok(output),
            Either::Left((Err(_), _)) => {
                self.loops.remove_tag(&tag);
                Err(Error::OnionRouteError(OnionRouteError::PipelineFailed))
            }
            Either::Right((_, _)) => {
                self.loops.remove_tag(&tag);
                Err(Error::OnionRouteError(OnionRouteError::PipelineTimedOut))
            }
        }
    }
}

/// Cross-platform handle for registering intermediate `invoke` backends and evaluating Kleisli
/// pipelines on both native and browser nodes (#834 Phase 2b).
#[derive(Clone)]
pub struct OnionPipelineHandle {
    runtime: OnionRuntime,
}

impl OnionPipelineHandle {
    pub(crate) const fn new(runtime: OnionRuntime) -> Self {
        Self { runtime }
    }

    /// Register an intermediate `invoke` backend `⟦(shape, backend)⟧` on this node's Σ-algebra.
    pub fn register_invoke(
        &self,
        shape: OnionInvokeShape,
        backend: OnionBackendName,
        policy: OnionInvokePolicy,
        backend_impl: impl OnionInvokeBackend + 'static,
    ) -> Result<()> {
        self.runtime
            .register_invoke(shape, backend, policy, backend_impl)
    }

    /// Select a multi-stage route for `pipeline` from `processor`'s directory and evaluate it at
    /// `pipeline.loop_class()` (#834 D4, L3).
    pub async fn invoke_pipeline(
        &self,
        processor: &crate::processor::Processor,
        pipeline: &OnionInvokePipeline,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        let route = processor.build_onion_pipeline_route(pipeline).await?;
        self.invoke_pipeline_over_route(&route, pipeline, input)
            .await
    }

    /// Evaluate `pipeline` over an already selected `route` at `pipeline.loop_class()` (#834 L3).
    pub async fn invoke_pipeline_over_route(
        &self,
        route: &OnionPipelineRoute,
        pipeline: &OnionInvokePipeline,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        self.invoke_pipeline_over_route_with_options(
            route,
            pipeline,
            pipeline.loop_class(),
            input,
            None,
        )
        .await
    }

    /// Evaluate `pipeline` over `route` with explicit loop `class` and optional caller `timeout`.
    pub async fn invoke_pipeline_over_route_with_options(
        &self,
        route: &OnionPipelineRoute,
        pipeline: &OnionInvokePipeline,
        class: OnionLoopClass,
        input: &[u8],
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>> {
        self.runtime
            .invoke_with_class_and_timeout(route, pipeline, class, input, timeout)
            .await
    }
}

/// `⟦−⟧` of the offer: one session interpretation per offered service this platform
/// interprets, all sharing the exit accounting and the link sender.
pub(super) fn exit_algebra(
    offer: Option<&OnionExitOffer>,
    accounting: &OnionExitAccounting,
    link_sender: &OnionLinkSender,
) -> OnionAlgebra {
    let Some(offer) = offer else {
        return OnionAlgebra::default();
    };
    offer
        .services()
        .iter()
        .fold(OnionAlgebra::default(), |algebra, service| {
            let policy = offer.policy().clone();
            if *service == OnionServiceName::https() {
                return algebra.register(
                    service.clone(),
                    OnionExitSessions::new(
                        OnionHttpsWorld::new(policy.clone(), accounting.clone()),
                        policy,
                        accounting.clone(),
                        link_sender.clone(),
                    ),
                );
            }
            #[cfg(rings_native)]
            {
                algebra.register(
                    service.clone(),
                    OnionExitSessions::new(
                        super::tcp::OnionTcpWorld::new(policy.clone(), accounting.clone()),
                        policy,
                        accounting.clone(),
                        link_sender.clone(),
                    ),
                )
            }
            #[cfg(not(rings_native))]
            {
                let _ = policy;
                algebra
            }
        })
}

#[cfg(all(test, rings_native))]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use super::*;
    use crate::onion::OnionExitPolicy;
    use crate::onion::OnionRole;
    use crate::tests::native::prepare_processor_with_onion_role;

    /// Regression (#1224 / #1229 item 5): `OnionRuntime::install` registers `ONION_CIRCUIT_NAMESPACE`
    /// and wires an exit algebra whose registered services equal the processor's offered services
    /// (`{}` for `Client` and `Relay`, and `offer.services()` for single- and dual-service `Exit`s),
    /// while `install_with` installs the caller-provided algebra verbatim.
    #[tokio::test]
    async fn test_install_registers_exact_offered_services_in_runtime_shell() -> Result<()> {
        let policy =
            OnionExitPolicy::from_target_strings(vec!["example.com:443".to_string()], Vec::new())?;
        let https_offer = OnionExitOffer::new([OnionServiceName::https()], policy.clone())?;
        let tcp_offer = OnionExitOffer::new([OnionServiceName::tcp()], policy.clone())?;
        let both_offer = OnionExitOffer::new(
            [OnionServiceName::tcp(), OnionServiceName::https()],
            policy.clone(),
        )?;

        for (role, expected) in [
            (OnionRole::Client, BTreeSet::new()),
            (OnionRole::Relay, BTreeSet::new()),
            (
                OnionRole::Exit(https_offer),
                BTreeSet::from([OnionServiceName::https()]),
            ),
            (
                OnionRole::Exit(tcp_offer),
                BTreeSet::from([OnionServiceName::tcp()]),
            ),
            (
                OnionRole::Exit(both_offer),
                BTreeSet::from([OnionServiceName::https(), OnionServiceName::tcp()]),
            ),
        ] {
            let processor = Arc::new(prepare_processor_with_onion_role(role).await);
            let extensions = Extensions::new(processor);
            let runtime = OnionRuntime::install(&extensions)?;
            assert!(extensions.contains(ONION_CIRCUIT_NAMESPACE));
            assert_eq!(runtime.registered_services, expected);
        }

        // `install_with` installs the caller-chosen algebra even on a `Client` processor.
        let client = Arc::new(prepare_processor_with_onion_role(OnionRole::Client).await);
        let custom_extensions = Extensions::new(client);
        let custom_offer = OnionExitOffer::new([OnionServiceName::https()], policy)?;
        let custom_runtime =
            OnionRuntime::install_with(&custom_extensions, |accounting, link_sender| {
                exit_algebra(Some(&custom_offer), accounting, link_sender)
            })?;
        assert_eq!(
            custom_runtime.registered_services,
            BTreeSet::from([OnionServiceName::https()])
        );
        Ok(())
    }
}
