//! Native TCP onion-exit service configuration.

use std::collections::BTreeSet;

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
        })
    }

    /// Build a native TCP exit config for the reserved `tcp` service.
    pub fn tcp(policy: OnionExitPolicy) -> Self {
        Self {
            services: vec![OnionServiceName::tcp()],
            policy,
        }
    }

    /// Return whether this exit may execute TCP payloads for `service`.
    pub fn allows_service(&self, service: &OnionServiceName) -> bool {
        self.services.iter().any(|candidate| candidate == service)
    }

    pub(super) fn policy(&self) -> &OnionExitPolicy {
        &self.policy
    }
}
