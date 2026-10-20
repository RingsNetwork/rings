//! Stable gateway health projection for CLI and RPC consumers.

use std::sync::Arc;
use std::sync::RwLock;

use ipnet::IpNet;
use serde::Deserialize;
use serde::Serialize;

use crate::FlowResetReason;
use crate::GatewayState;

/// Diagnostic of the initial exit observation, before discovery has reported once.
pub(crate) const EXIT_DISCOVERY_PENDING: &str = "Onion exit discovery has not completed";

/// Current availability of a compatible onion TCP exit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitAvailability {
    /// Exit discovery has not completed.
    Unknown,
    /// At least one compatible exit is currently eligible.
    Available,
    /// No compatible exit is currently eligible.
    Unavailable,
}

/// Observed compatible-exit service, with the diagnostic every non-available observation owns.
///
/// Projection law: [`Self::reason`] is `None` exactly when [`Self::availability`] is
/// [`ExitAvailability::Available`]. An active gateway is `Degraded` exactly when its exit is not
/// available, so a degraded status always names its reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExitObservation {
    /// At least one compatible exit is currently eligible.
    Available,
    /// Exit availability is not currently known, with the cause.
    Unknown(String),
    /// No compatible exit is currently eligible, with the cause.
    Unavailable(String),
}

impl ExitObservation {
    /// Observation held before exit discovery has reported.
    pub fn pending() -> Self {
        Self::Unknown(EXIT_DISCOVERY_PENDING.to_string())
    }

    /// Project the stable availability class.
    pub const fn availability(&self) -> ExitAvailability {
        match self {
            Self::Available => ExitAvailability::Available,
            Self::Unknown(_) => ExitAvailability::Unknown,
            Self::Unavailable(_) => ExitAvailability::Unavailable,
        }
    }

    /// Return the diagnostic owned by a non-available observation.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unknown(reason) | Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// Stable external health vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatewayHealth {
    /// Gateway is not configured or is stopped cleanly.
    Inactive,
    /// Startup or shutdown is in progress.
    Transitioning,
    /// Packets are admitted and a compatible exit is available.
    Active,
    /// The gateway is running without full exit service.
    Degraded,
    /// Gateway failure is independently observable from process health.
    Failed,
}

/// RPC-safe snapshot of gateway health.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GatewayStatus {
    /// External health projection.
    pub health: GatewayHealth,
    /// Platform interface name when one exists.
    pub interface: Option<String>,
    /// Normalized destination prefixes explicitly selected for packet capture.
    pub capture_routes: Vec<IpNet>,
    /// Compatible onion exit availability.
    pub exit_availability: ExitAvailability,
    /// Number of currently tracked TCP flows.
    pub active_flows: usize,
    /// Stable diagnostic for degraded or failed state.
    pub reason: Option<String>,
    /// Cause of the most recent captured-flow reset since the gateway last started.
    pub last_flow_reset: Option<FlowResetReason>,
}

/// Cloneable, read-only gateway status capability for CLI and RPC handlers.
///
/// The runtime remains the only writer. Readers recover the last complete snapshot if a
/// different reader panicked while holding the standard-library lock, so inspection cannot make
/// packet forwarding fail.
#[derive(Clone)]
pub struct GatewayStatusHandle {
    inner: Arc<RwLock<GatewayStatus>>,
}

impl GatewayStatusHandle {
    pub(crate) fn new(status: GatewayStatus) -> Self {
        Self {
            inner: Arc::new(RwLock::new(status)),
        }
    }

    pub(crate) fn publish(&self, status: GatewayStatus) {
        match self.inner.write() {
            Ok(mut current) => *current = status,
            Err(poisoned) => *poisoned.into_inner() = status,
        }
    }

    /// Return one internally consistent status snapshot.
    pub fn snapshot(&self) -> GatewayStatus {
        match self.inner.read() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl GatewayStatus {
    /// Build a stable status projection from internal lifecycle and exit observation.
    pub fn from_state(
        state: GatewayState,
        interface: Option<String>,
        capture_routes: Vec<IpNet>,
        exit: &ExitObservation,
        active_flows: usize,
        last_flow_reset: Option<FlowResetReason>,
    ) -> Self {
        let exit_availability = exit.availability();
        let health = match (state, exit_availability) {
            (GatewayState::Stopped, _) => GatewayHealth::Inactive,
            (GatewayState::Starting | GatewayState::Stopping, _) => GatewayHealth::Transitioning,
            (GatewayState::Active, ExitAvailability::Available) => GatewayHealth::Active,
            (GatewayState::Active, _) => GatewayHealth::Degraded,
            (GatewayState::Failed, _) => GatewayHealth::Failed,
        };
        Self {
            health,
            interface,
            capture_routes,
            exit_availability,
            active_flows,
            reason: exit.reason().map(str::to_string),
            last_flow_reset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_independent_exit_loss_is_degraded() {
        let status = GatewayStatus::from_state(
            GatewayState::Active,
            Some("tun-rings0".to_string()),
            vec!["198.18.0.0/15".parse().expect("test capture route")],
            &ExitObservation::Unavailable("no compatible onion TCP exit".to_string()),
            0,
            None,
        );
        assert_eq!(status.health, GatewayHealth::Degraded);
    }

    #[test]
    fn test_degraded_status_always_names_its_reason() {
        for exit in [
            ExitObservation::pending(),
            ExitObservation::Unknown("discovery failed".to_string()),
            ExitObservation::Unavailable("no exit".to_string()),
        ] {
            let status =
                GatewayStatus::from_state(GatewayState::Active, None, Vec::new(), &exit, 0, None);
            assert_eq!(status.health, GatewayHealth::Degraded);
            assert_eq!(status.reason.as_deref(), exit.reason());
            assert!(status.reason.is_some());
        }
        let active = GatewayStatus::from_state(
            GatewayState::Active,
            None,
            Vec::new(),
            &ExitObservation::Available,
            0,
            None,
        );
        assert_eq!(active.health, GatewayHealth::Active);
        assert_eq!(active.reason, None);
    }

    #[test]
    fn status_handle_returns_the_latest_complete_snapshot() {
        let initial = GatewayStatus::from_state(
            GatewayState::Stopped,
            None,
            Vec::new(),
            &ExitObservation::pending(),
            0,
            None,
        );
        let handle = GatewayStatusHandle::new(initial);
        let active = GatewayStatus::from_state(
            GatewayState::Active,
            Some("rings0".to_string()),
            vec!["198.18.0.0/15".parse().expect("test capture route")],
            &ExitObservation::Available,
            2,
            Some(FlowResetReason::IdleTimeout),
        );

        handle.publish(active.clone());

        assert_eq!(handle.snapshot(), active);
    }
}
