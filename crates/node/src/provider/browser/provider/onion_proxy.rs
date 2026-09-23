use super::build_browser_onion_proxy_route;
use super::BrowserOnionProxy;
use super::BrowserOnionProxyResponse;
use crate::error::Result as NodeResult;
use crate::onion::https::OnionHttpsCall;
use crate::onion::https::OnionHttpsClientRequest;
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
        let (target, call) = OnionHttpsCall::from_url(url, request)?;
        let route = self.build_route(target).await?;
        let response = self
            .client
            .request(self.scope.clone(), &route, call)
            .await?;
        Ok(BrowserOnionProxyResponse { response, route })
    }
}

#[cfg(test)]
mod tests;
