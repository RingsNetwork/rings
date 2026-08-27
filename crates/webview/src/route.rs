//! Trusted host routing policy for a controlled webview origin.

use url::Url;

use crate::error::Result;
use crate::error::WebviewError;
use crate::types::GatewayRequestKind;
use crate::url::GatewayPrefix;
use crate::url::TargetUrl;

/// A request-routing policy implemented by the WebView host, service worker, or equivalent.
///
/// The host supplies `source_target` from its frame state, never from an untrusted page header.
/// Runtime requests retain that target so the transport layer can apply virtual CORS after the
/// request is forwarded through the controlled gateway origin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GatewayRoutePolicy {
    controlled_origin: Url,
    gateway_prefix: GatewayPrefix,
}

impl GatewayRoutePolicy {
    /// Build a policy for one Rings-controlled browser origin and gateway path prefix.
    pub fn new(controlled_origin: Url, gateway_prefix: GatewayPrefix) -> Result<Self> {
        if !is_controlled_origin(&controlled_origin) {
            return Err(WebviewError::InvalidControlledOrigin(
                controlled_origin.to_string(),
            ));
        }
        Ok(Self {
            controlled_origin,
            gateway_prefix,
        })
    }

    /// Build the controlled-origin URL that represents `target`.
    pub fn gateway_url(&self, target: &Url) -> Result<Url> {
        self.controlled_origin
            .join(self.gateway_prefix.encode(target).as_str())
            .map_err(WebviewError::Url)
    }

    /// Decide how a host must handle one browser request before the browser opens a connection.
    ///
    /// `source_target` is the target URL associated with the initiating frame. It is required for
    /// fetch and XHR virtual CORS enforcement, but navigation and static subresources may cross
    /// target origins. The host must treat an error as a rejection and never fall back to
    /// `requested_url`.
    pub fn route(
        &self,
        requested_url: &Url,
        source_target: Option<&TargetUrl>,
        kind: GatewayRequestKind,
    ) -> Result<GatewayRoute> {
        if runtime_request_lacks_source_target(source_target, kind) {
            return Ok(GatewayRoute::Reject(
                GatewayRouteRejection::MissingRuntimeSourceTarget,
            ));
        }
        if self.is_controlled_url(requested_url) && !self.is_gateway_url(requested_url) {
            return Ok(GatewayRoute::AllowControlled);
        }
        let target = self.target_from_requested_url(requested_url, kind)?;
        self.route_target(requested_url, target)
    }

    fn target_from_requested_url(
        &self,
        requested_url: &Url,
        kind: GatewayRequestKind,
    ) -> Result<TargetUrl> {
        if self.is_controlled_url(requested_url) {
            let target = self.gateway_prefix.decode_path(requested_url.path())?;
            let Some(form_query) = requested_url.query() else {
                return Ok(target);
            };
            if kind != GatewayRequestKind::Navigation {
                return Err(WebviewError::InvalidGatewayUrl(requested_url.to_string()));
            }

            // Native GET form submissions append fields to the gateway URL rather than to its
            // percent-encoded target. Only a document navigation can use that representation.
            let mut target_url = target.into_url();
            let target_query = target_url
                .query()
                .map(|query| format!("{query}&{form_query}"))
                .unwrap_or_else(|| form_query.to_string());
            target_url.set_query(Some(&target_query));
            return TargetUrl::parse(target_url.as_str());
        }
        TargetUrl::parse(requested_url.as_str())
    }

    fn route_target(&self, requested_url: &Url, target: TargetUrl) -> Result<GatewayRoute> {
        if self.is_controlled_url(requested_url) {
            return Ok(GatewayRoute::Serve(target));
        }
        Ok(GatewayRoute::Redirect(self.gateway_url(target.as_url())?))
    }

    fn is_controlled_url(&self, url: &Url) -> bool {
        url.origin() == self.controlled_origin.origin()
    }

    fn is_gateway_url(&self, url: &Url) -> bool {
        url.path().starts_with(self.gateway_prefix.as_str())
    }
}

/// A complete host action for one browser request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayRoute {
    /// Allow a Rings-owned shell or static asset to load without forwarding it upstream.
    AllowControlled,
    /// Serve a decoded target through the `GatewayTransport` boundary.
    Serve(TargetUrl),
    /// Redirect an external navigation or subresource to this controlled gateway URL.
    Redirect(Url),
    /// Reject the request before the browser can open a remote connection.
    Reject(GatewayRouteRejection),
}

/// The named reason a host must reject a browser request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayRouteRejection {
    /// The host did not provide a trusted initiating-frame target for fetch or XHR.
    MissingRuntimeSourceTarget,
}

fn is_controlled_origin(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
}

fn runtime_request_lacks_source_target(
    source_target: Option<&TargetUrl>,
    kind: GatewayRequestKind,
) -> bool {
    matches!(kind, GatewayRequestKind::Fetch | GatewayRequestKind::Xhr) && source_target.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Result<GatewayRoutePolicy> {
        GatewayRoutePolicy::new(
            Url::parse("https://webview.rings.test/")?,
            GatewayPrefix::new("/webview/")?,
        )
    }

    #[test]
    fn test_external_navigation_redirects_to_controlled_gateway_url() -> Result<()> {
        let policy = policy()?;
        let requested = Url::parse("https://example.test/docs/index.html")?;

        let route = policy.route(&requested, None, GatewayRequestKind::Navigation)?;

        assert_eq!(
            route,
            GatewayRoute::Redirect(Url::parse(
                "https://webview.rings.test/webview/https%3A%2F%2Fexample%2Etest%2Fdocs%2Findex%2Ehtml"
            )?)
        );
        Ok(())
    }

    #[test]
    fn test_controlled_gateway_url_decodes_to_target_transport_route() -> Result<()> {
        let policy = policy()?;
        let target = Url::parse("https://example.test/docs/index.html")?;
        let requested = policy.gateway_url(&target)?;

        let route = policy.route(&requested, None, GatewayRequestKind::Navigation)?;

        assert_eq!(
            route,
            GatewayRoute::Serve(TargetUrl::parse(target.as_str())?)
        );
        Ok(())
    }

    #[test]
    fn test_navigation_form_query_is_merged_into_gateway_target() -> Result<()> {
        let policy = policy()?;
        let target = Url::parse("https://www.google.com/search?source=hp")?;
        let mut requested = policy.gateway_url(&target)?;
        requested.set_query(Some("q=test+query&hl=en"));

        let route = policy.route(&requested, None, GatewayRequestKind::Navigation)?;

        assert_eq!(
            route,
            GatewayRoute::Serve(TargetUrl::parse(
                "https://www.google.com/search?source=hp&q=test+query&hl=en"
            )?)
        );
        Ok(())
    }

    #[test]
    fn test_gateway_outer_query_is_rejected_for_non_navigation_requests() -> Result<()> {
        let policy = policy()?;
        let target = Url::parse("https://example.test/asset.css")?;
        let mut requested = policy.gateway_url(&target)?;
        requested.set_query(Some("cache_bust=1"));

        assert!(matches!(
            policy.route(&requested, None, GatewayRequestKind::Subresource),
            Err(WebviewError::InvalidGatewayUrl(_))
        ));
        Ok(())
    }

    #[test]
    fn test_cross_target_fetch_is_served_for_virtual_cors_evaluation() -> Result<()> {
        let policy = policy()?;
        let source = TargetUrl::parse("https://app.example.test/index.html")?;
        let target = Url::parse("https://bank.example.test/account")?;
        let requested = policy.gateway_url(&target)?;

        let route = policy.route(&requested, Some(&source), GatewayRequestKind::Fetch)?;

        assert_eq!(
            route,
            GatewayRoute::Serve(TargetUrl::parse(target.as_str())?)
        );
        Ok(())
    }

    #[test]
    fn test_cross_target_direct_fetch_is_redirected_through_the_gateway() -> Result<()> {
        let policy = policy()?;
        let source = TargetUrl::parse("https://app.example.test/index.html")?;
        let requested = Url::parse("https://bank.example.test/account")?;

        let route = policy.route(&requested, Some(&source), GatewayRequestKind::Fetch)?;

        assert_eq!(
            route,
            GatewayRoute::Redirect(policy.gateway_url(&requested)?)
        );
        Ok(())
    }

    #[test]
    fn test_same_target_xhr_is_served_through_gateway() -> Result<()> {
        let policy = policy()?;
        let source = TargetUrl::parse("https://app.example.test/docs/index.html")?;
        let target = Url::parse("https://app.example.test/api/data")?;
        let requested = policy.gateway_url(&target)?;

        let route = policy.route(&requested, Some(&source), GatewayRequestKind::Xhr)?;

        assert_eq!(
            route,
            GatewayRoute::Serve(TargetUrl::parse(target.as_str())?)
        );
        Ok(())
    }

    #[test]
    fn test_runtime_requests_without_a_trusted_source_target_are_rejected() -> Result<()> {
        let policy = policy()?;
        let requested = Url::parse("https://app.example.test/api/data")?;

        let route = policy.route(&requested, None, GatewayRequestKind::Fetch)?;

        assert_eq!(
            route,
            GatewayRoute::Reject(GatewayRouteRejection::MissingRuntimeSourceTarget)
        );
        Ok(())
    }

    #[test]
    fn test_cross_target_subresources_redirect_without_granting_script_read_access() -> Result<()> {
        let policy = policy()?;
        let source = TargetUrl::parse("https://app.example.test/index.html")?;
        let requested = Url::parse("https://cdn.example.test/site.css")?;

        let route = policy.route(&requested, Some(&source), GatewayRequestKind::Subresource)?;

        assert_eq!(
            route,
            GatewayRoute::Redirect(policy.gateway_url(&requested)?)
        );
        Ok(())
    }

    #[test]
    fn test_controlled_origin_configuration_excludes_paths_and_credentials() -> Result<()> {
        for value in [
            "https://webview.rings.test/app",
            "https://user@webview.rings.test/",
            "file:///tmp/webview",
        ] {
            assert!(matches!(
                GatewayRoutePolicy::new(Url::parse(value)?, GatewayPrefix::new("/webview/")?),
                Err(WebviewError::InvalidControlledOrigin(_))
            ));
        }
        Ok(())
    }

    #[test]
    fn test_controlled_shell_assets_remain_local() -> Result<()> {
        let policy = policy()?;
        let requested = Url::parse("https://webview.rings.test/assets/app.js")?;

        let route = policy.route(&requested, None, GatewayRequestKind::Subresource)?;

        assert_eq!(route, GatewayRoute::AllowControlled);
        Ok(())
    }

    #[test]
    fn test_runtime_requests_to_controlled_assets_still_require_a_trusted_source() -> Result<()> {
        let policy = policy()?;
        let requested = Url::parse("https://webview.rings.test/assets/app.js")?;

        let route = policy.route(&requested, None, GatewayRequestKind::Xhr)?;

        assert_eq!(
            route,
            GatewayRoute::Reject(GatewayRouteRejection::MissingRuntimeSourceTarget)
        );
        Ok(())
    }
}
