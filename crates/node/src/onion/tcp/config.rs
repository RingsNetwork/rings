//! Native TCP onion-exit service configuration.

use std::collections::BTreeSet;

use reqwest::Url;

use crate::error::Error;
use crate::error::Result;
use crate::onion::OnionExitPolicy;
use crate::onion::OnionExitService;
use crate::onion::OnionExitTransport;
use crate::onion::OnionServiceName;

/// Native TCP exit capabilities installed into the onion circuit data plane.
///
/// Invariant: `services` is non-empty and every name was derived from an advertised
/// [`OnionExitService`] whose transport is [`OnionExitTransport::Tcp`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeOnionTcpExitConfig {
    services: Vec<OnionServiceName>,
    policy: OnionExitPolicy,
    https_proxy: Option<String>,
}

impl NativeOnionTcpExitConfig {
    /// Build a native TCP exit config from advertised registry services.
    pub fn new(
        services: impl IntoIterator<Item = OnionExitService>,
        policy: OnionExitPolicy,
    ) -> Result<Self> {
        let mut service_names = BTreeSet::new();
        for service in services {
            if service.transport != OnionExitTransport::Tcp {
                return Err(Error::InvalidConfig(format!(
                    "native onion TCP exit cannot serve {:?} over {:?}",
                    service.name, service.transport
                )));
            }
            service_names.insert(service.name);
        }
        if service_names.is_empty() {
            return Err(Error::InvalidConfig(
                "native onion TCP exit requires at least one TCP service".to_string(),
            ));
        }
        Ok(Self {
            services: service_names.into_iter().collect(),
            policy,
            https_proxy: None,
        })
    }

    /// Build a native TCP exit config for the reserved `tcp` service.
    pub fn tcp(policy: OnionExitPolicy) -> Self {
        Self {
            services: vec![OnionServiceName::tcp()],
            policy,
            https_proxy: None,
        }
    }

    /// Explicitly delegate eligible synthetic-DNS HTTPS targets to this operator proxy.
    ///
    /// Ambient process proxy variables are intentionally ignored by onion exits: enabling a new
    /// egress trust boundary must be an explicit node capability.
    pub fn with_https_proxy(mut self, proxy: impl AsRef<str>) -> Result<Self> {
        let proxy = proxy.as_ref().trim();
        let parsed = Url::parse(proxy).map_err(|_| {
            Error::InvalidConfig("native onion HTTPS proxy must be an absolute URL".to_string())
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(Error::InvalidConfig(
                "native onion HTTPS proxy must use http or https with a host".to_string(),
            ));
        }
        self.https_proxy = Some(proxy.to_string());
        Ok(self)
    }

    /// Return whether this exit may execute TCP payloads for `service`.
    pub fn allows_service(&self, service: &OnionServiceName) -> bool {
        self.services.iter().any(|candidate| candidate == service)
    }

    pub(super) fn policy(&self) -> &OnionExitPolicy {
        &self.policy
    }

    pub(super) fn https_proxy(&self) -> Option<&str> {
        self.https_proxy.as_deref()
    }
}
