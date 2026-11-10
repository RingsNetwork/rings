//! The native handle of a node's onion runtime: `tcp` streams, `https` requests, and multi-stage
//! `invoke` Kleisli pipelines over client-sealed loops.

use std::time::Duration;

use crate::error::Error;
use crate::error::Result;
use crate::onion::circuit::OnionInvokeBackend;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::invoke_descriptor::OnionInvokePolicy;
use crate::onion::pipeline::OnionInvokePipeline;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::route::OnionPipelineRoute;
use crate::onion::runtime::OnionRuntime;
use crate::onion::session::client::OnionCreditWindow;
use crate::onion::session::dial::OnionSessionRequest;
use crate::onion::signature::OnionBackendName;
use crate::onion::signature::OnionInvokeShape;
use crate::onion::tcp::NativeOnionOpenStream;
use crate::onion::OnionLoopClass;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
use crate::processor::Processor;

/// Native handle for TCP streams, HTTPS requests, and Kleisli pipelines over onion loops.
#[derive(Clone)]
pub struct NativeOnionCircuitHandle {
    /// The installed runtime.
    runtime: OnionRuntime,
}

impl NativeOnionCircuitHandle {
    /// The native handle of `runtime`, which a provider installed once in its registry
    /// ([`Provider::onion_circuit`](crate::provider::Provider::onion_circuit)).
    pub(crate) const fn new(runtime: OnionRuntime) -> Self {
        Self { runtime }
    }

    /// Register an intermediate `invoke` backend `⟦(shape, backend)⟧` on this node's Σ-algebra
    /// (#834 D2). If the node's role already advertises `(shape, backend)` in its
    /// [`crate::onion::OnionInvokeOffer`], that signed quota policy takes precedence over `policy`.
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
        processor: &Processor,
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

    /// Evaluate `pipeline` over `route` at explicit loop `class` and optional caller `timeout`
    /// (clamped to the loop's expiry `x`).
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

    /// Select a `tcp` route to `target` from `processor`'s directory and open a session over it
    /// at [`OnionLoopClass::DEFAULT`].
    pub async fn open_tcp_target(
        &self,
        processor: &Processor,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        self.open_tcp_target_with_class(processor, target, OnionLoopClass::DEFAULT)
            .await
    }

    /// Select a `tcp` route to `target` from `processor`'s directory and open a session of loop
    /// class `class` (`b`) over it.
    pub async fn open_tcp_target_with_class(
        &self,
        processor: &Processor,
        target: OnionProxyTarget,
        class: OnionLoopClass,
    ) -> Result<NativeOnionOpenStream> {
        let route = processor
            .build_onion_proxy_route(OnionProxyProtocol::TcpConnect, target)
            .await?;
        self.open_tcp_stream_with_class(route.route, route.target, class)
            .await
    }

    /// Open a `tcp` session over `route` at [`OnionLoopClass::DEFAULT`] and wait until the exit
    /// has connected `target`.
    pub async fn open_tcp_stream(
        &self,
        route: OnionRoute,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        self.open_tcp_stream_with_class(route, target, OnionLoopClass::DEFAULT)
            .await
    }

    /// Open a `tcp` session of loop class `class` (`b`) over `route` and wait until the exit has
    /// connected `target`.
    pub async fn open_tcp_stream_with_class(
        &self,
        route: OnionRoute,
        target: OnionProxyTarget,
        class: OnionLoopClass,
    ) -> Result<NativeOnionOpenStream> {
        if route.service_name() != &OnionServiceName::tcp() {
            return Err(Error::OnionRouteError(
                OnionRouteError::PayloadServiceMismatch {
                    payload_service: OnionServiceName::tcp().as_str().to_string(),
                    route_service: route.service().to_string(),
                },
            ));
        }
        self.runtime
            .open(OnionSessionRequest {
                route,
                symbol: OnionServiceName::tcp(),
                target,
                class,
                window: OnionCreditWindow::DEFAULT,
            })
            .await
            .map(NativeOnionOpenStream::new)
    }

    /// Send `call` to `route.target` over `route` and wait for the exit's response.
    pub async fn request_https(
        &self,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
    ) -> Result<OnionHttpsResponse> {
        OnionHttpsClient::new(self.runtime.clone())
            .request(route, call)
            .await
    }

    /// Send `call` to `route.target` over `route` in a session of loop class `class` (`b`) and
    /// wait for the exit's response.
    pub async fn request_https_with_class(
        &self,
        route: &OnionProxyRoute,
        call: OnionHttpsCall,
        class: OnionLoopClass,
    ) -> Result<OnionHttpsResponse> {
        OnionHttpsClient::new(self.runtime.clone())
            .request_with_class(route, call, class)
            .await
    }
}

#[cfg(test)]
mod tests;
