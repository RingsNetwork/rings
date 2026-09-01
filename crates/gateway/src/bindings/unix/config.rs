//! Serializable request surface for the narrow Unix gateway configuration helper.

use serde::Deserialize;
use serde::Serialize;

use crate::GatewayPlan;

/// Request accepted by the foreground `gateway-config-unix` helper.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum UnixConfigRequest {
    /// Reconcile stale state and establish a new tunnel lease.
    Establish {
        /// Fully validated declarative tunnel plan.
        plan: GatewayPlan,
    },
    /// Tear down the named linear cleanup lease.
    Teardown {
        /// Lease identifier returned by establishment.
        lease_id: String,
    },
}

/// Response returned by the Unix gateway configuration helper.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum UnixConfigResponse {
    /// Tunnel resources were established.
    Established {
        /// Opaque cleanup lease identifier.
        lease_id: String,
        /// Created TUN or utun interface.
        interface_name: String,
    },
    /// Teardown completed.
    TornDown,
    /// A named helper operation failed.
    Failed {
        /// Stable operation label.
        operation: String,
        /// Platform diagnostic.
        message: String,
    },
}
