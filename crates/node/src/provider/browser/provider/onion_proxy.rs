use futures::future::Either;
use futures::FutureExt;
use rings_core::utils::js_utils;

use super::build_browser_onion_proxy_route;
use super::BrowserOnionProxy;
use super::BrowserOnionProxyResponse;
use crate::error::Error;
use crate::error::Result as NodeResult;
use crate::onion::circuit::encode_initial_forward;
use crate::onion::circuit::route_first_hop;
use crate::onion::circuit::OnionClientReturn;
use crate::onion::circuit::ONION_CIRCUIT_NAMESPACE;
use crate::onion::https::encode_https_payload;
use crate::onion::https::OnionHttpsClientRequest;
use crate::onion::https::OnionHttpsPayload;
use crate::onion::proxy::OnionProxyRoute;
use crate::onion::proxy::OnionProxyTarget;

impl BrowserOnionProxy {
    async fn build_route(&self, target: OnionProxyTarget) -> NodeResult<OnionProxyRoute> {
        build_browser_onion_proxy_route(
            self.processor.clone(),
            self.config.clone(),
            target,
            self.directory_endpoint.clone(),
        )
        .await
    }

    /// Build one typed HTTPS onion route without crossing a JavaScript promise boundary.
    pub async fn route_http(&self, target_authority: &str) -> NodeResult<OnionProxyRoute> {
        let target = OnionProxyTarget::parse_authority(target_authority)?;
        self.build_route(target).await
    }

    /// Send one typed HTTPS request through this proxy.
    ///
    /// Dropping the returned future cancels its pending circuit immediately. Browser frontends
    /// should use this method when their own request lifecycle can be cancelled.
    pub async fn request_http(
        &self,
        url: &str,
        request: OnionHttpsClientRequest,
    ) -> NodeResult<BrowserOnionProxyResponse> {
        let (target, request) = crate::onion::https::client_request_from_url(url, request)?;
        let proxy_route = self.build_route(target).await?;
        let first_hop = route_first_hop(&proxy_route.route)?;
        let client_return =
            OnionClientReturn::new(self.processor.session_sk().session_public_key());
        let pending_request = self.runtime.begin_request(
            first_hop,
            proxy_route.route.exit().clone(),
            client_return.return_id,
        )?;
        let id = pending_request.id();
        let request_payload = encode_https_payload(OnionHttpsPayload::Request(request))?;
        let (to, payload) =
            encode_initial_forward(client_return, &proxy_route.route, id, request_payload)?;
        let envelope =
            crate::extension::ext::Envelope::new(ONION_CIRCUIT_NAMESPACE.to_string(), payload);
        self.processor.send_direct_envelope(to, &envelope).await?;

        let response = pending_request.fuse();
        let timeout = js_utils::window_sleep(30_000).fuse();
        futures::pin_mut!(response, timeout);
        let response = match futures::future::select(response, timeout).await {
            Either::Left((result, _)) => match result {
                Ok(result) => result?,
                Err(_) => {
                    return Err(Error::HttpRequestError(
                        "onion HTTPS proxy response channel closed".to_string(),
                    ));
                }
            },
            Either::Right((_, _)) => {
                return Err(Error::OnionProxyRequestTimedOut);
            }
        };
        Ok(BrowserOnionProxyResponse {
            response,
            route: proxy_route,
        })
    }
}
