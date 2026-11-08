//! The native handle of a node's onion runtime: `tcp` streams and `https` requests over loops.
//!
//! A native node installs one runtime (`onion::runtime`) whose exit algebra registers exactly the
//! configured exit services `Σ_n`, each over its own world: `tcp` over sockets, `https` over the
//! native fetch. One interpretation per symbol, never a fallback to another (#834 D1′).
//!
//! ```text
//! open_tcp_target[_with_class](t[, b])         ─▶ route(tcp, t) ─▶ open_tcp_stream_with_class
//! open_tcp_stream[_with_class](route, t[, b])  ─▶ session(tcp, t, b) ─▶ NativeOnionOpenStream ─relay─▶ local stream
//! request_https[_with_class](route, call[, b]) ─▶ session(https, route.target, b) ─▶ response
//! ```

use crate::error::Error;
use crate::error::Result;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClient;
use crate::onion::https::OnionHttpsResponse;
use crate::onion::proxy::OnionProxyProtocol;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::runtime::OnionRuntime;
use crate::onion::session::client::OnionCreditWindow;
use crate::onion::session::dial::OnionSessionRequest;
use crate::onion::tcp::NativeOnionOpenStream;
use crate::onion::OnionLoopClass;
use crate::onion::OnionProxyTarget;
use crate::onion::OnionRoute;
use crate::onion::OnionRouteError;
use crate::onion::OnionServiceName;
use crate::processor::Processor;

/// Native handle for TCP streams and HTTPS requests over onion loops.
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

    /// Select a `tcp` route to `target` from `processor`'s directory and open a session over it
    /// at [`OnionLoopClass::DEFAULT`] (`16 KiB`, [`Self::open_tcp_stream`]): the default pipeline
    /// of byte-tunnel ingresses, HTTP CONNECT and the packet gateway alike.
    ///
    /// To choose a non-default loop class `b`, call [`Self::open_tcp_target_with_class`].
    ///
    /// # Errors
    ///
    /// The route selection's failure, or as for [`Self::open_tcp_stream`].
    pub async fn open_tcp_target(
        &self,
        processor: &Processor,
        target: OnionProxyTarget,
    ) -> Result<NativeOnionOpenStream> {
        self.open_tcp_target_with_class(processor, target, OnionLoopClass::DEFAULT)
            .await
    }

    /// Select a `tcp` route to `target` from `processor`'s directory and open a session of loop
    /// class `class` (`b`) over it ([`Self::open_tcp_stream_with_class`]).
    ///
    /// Every hop on the loop sees `b` (#834 leakage table), and each cell of the loop costs
    /// `b / 16 KiB` admission units ([`OnionLoopClass::units`]).
    ///
    /// # Errors
    ///
    /// The route selection's failure, or as for [`Self::open_tcp_stream_with_class`].
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

    /// Open a `tcp` session over `route` at [`OnionLoopClass::DEFAULT`] (`16 KiB`) and wait until
    /// the exit has connected `target`.
    ///
    /// To choose a non-default loop class `b`, call [`Self::open_tcp_stream_with_class`].
    ///
    /// # Errors
    ///
    /// [`OnionRouteError::PayloadServiceMismatch`] for a route selected for another symbol, the
    /// exit's refusal, or a timeout.
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
    ///
    /// Every hop on the loop sees `b` (#834 leakage table), and each cell of the loop costs
    /// `b / 16 KiB` admission units ([`OnionLoopClass::units`]).
    ///
    /// # Errors
    ///
    /// [`OnionRouteError::PayloadServiceMismatch`] for a route selected for another symbol, the
    /// exit's refusal, or a timeout.
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
    ///
    /// Obtain the target and call from [`OnionHttpsCall::from_url`], then select `route` for that
    /// target under [`OnionProxyProtocol::HttpsProxy`]. The session opens at
    /// [`OnionHttpsCall::class`] ([`OnionLoopClass::DEFAULT`], `16 KiB`, unless chosen via
    /// [`OnionHttpsClientRequest::class`](crate::onion::https::OnionHttpsClientRequest::class) or
    /// [`OnionHttpsCall::with_class`]); to override the class at send time, call
    /// [`Self::request_https_with_class`].
    /// Dropping the future ends the session, and a silent exit yields
    /// [`Error::OnionProxyRequestTimedOut`].
    ///
    /// # Errors
    ///
    /// The session's refusal or failure, the exit's reported failure, or the timeout.
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
    ///
    /// Every hop on the loop sees `b` (#834 leakage table), and each cell of the loop costs
    /// `b / 16 KiB` admission units ([`OnionLoopClass::units`]).
    /// Dropping the future ends the session, and a silent exit yields
    /// [`Error::OnionProxyRequestTimedOut`].
    ///
    /// # Errors
    ///
    /// The session's refusal or failure, the exit's reported failure, or the timeout.
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
