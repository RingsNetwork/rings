//! Stable gateway health projection for CLI and RPC consumers.

use std::sync::Arc;
use std::sync::RwLock;

use ipnet::IpNet;
use serde::Deserialize;
use serde::Serialize;

use crate::GatewayState;

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
    /// Build a stable status projection from internal lifecycle and exit availability.
    pub fn from_state(
        state: GatewayState,
        interface: Option<String>,
        capture_routes: Vec<IpNet>,
        exit_availability: ExitAvailability,
        active_flows: usize,
        reason: Option<String>,
    ) -> Self {
        let health = match (state, exit_availability) {
            (GatewayState::Stopped, _) => GatewayHealth::Inactive,
            (GatewayState::Starting | GatewayState::Stopping, _) => GatewayHealth::Transitioning,
            (GatewayState::Active, ExitAvailability::Available) => GatewayHealth::Active,
            (GatewayState::Active | GatewayState::Degraded, _) => GatewayHealth::Degraded,
            (GatewayState::Failed, _) => GatewayHealth::Failed,
        };
        Self {
            health,
            interface,
            capture_routes,
            exit_availability,
            active_flows,
            reason,
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
            ExitAvailability::Unavailable,
            0,
            Some("no compatible onion TCP exit".to_string()),
        );
        assert_eq!(status.health, GatewayHealth::Degraded);
    }

    #[test]
    fn status_handle_returns_the_latest_complete_snapshot() {
        let initial = GatewayStatus::from_state(
            GatewayState::Stopped,
            None,
            Vec::new(),
            ExitAvailability::Unknown,
            0,
            None,
        );
        let handle = GatewayStatusHandle::new(initial);
        let active = GatewayStatus::from_state(
            GatewayState::Active,
            Some("rings0".to_string()),
            vec!["198.18.0.0/15".parse().expect("test capture route")],
            ExitAvailability::Available,
            2,
            None,
        );

        handle.publish(active.clone());

        assert_eq!(handle.snapshot(), active);
    }
}
