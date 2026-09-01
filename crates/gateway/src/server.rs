//! Pure gateway lifecycle model.

use crate::ExitAvailability;
use crate::FlowEvent;
use crate::FlowId;
use crate::FlowState;
use crate::FlowTable;
use crate::GatewayConfig;
use crate::GatewayError;
use crate::GatewayStatus;
use crate::GatewayTransitionError;

/// Events accepted by the gateway lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayEvent {
    /// Begin establishing interface state and explicit capture routes.
    Start,
    /// All selected capture resources are installed; packet admission may begin.
    AdmitPackets,
    /// A recoverable dependency failure reduced service.
    Degrade,
    /// All dependencies recovered.
    Recover,
    /// An unrecoverable failure requires fail-closed cleanup.
    Fail,
    /// Begin draining flows and reconciling platform resources.
    Stop,
    /// Cleanup finished.
    FinishStop,
}

/// Observable gateway lifecycle independent from process health.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayState {
    /// No interface or route is owned.
    Stopped,
    /// Platform state is being established; packets are not admitted.
    Starting,
    /// The gateway admits packets and has usable exit capacity.
    Active,
    /// The gateway remains observable but cannot currently provide full service.
    Degraded,
    /// Flow draining and platform cleanup are in progress.
    Stopping,
    /// The gateway failed closed and requires reconciliation before restart.
    Failed,
}

impl GatewayState {
    /// Apply one deterministic lifecycle event.
    pub fn transition(self, event: GatewayEvent) -> Result<Self, GatewayTransitionError> {
        match (self, event) {
            (Self::Stopped, GatewayEvent::Start) => Ok(Self::Starting),
            (Self::Starting, GatewayEvent::AdmitPackets) => Ok(Self::Active),
            (Self::Active, GatewayEvent::Degrade) => Ok(Self::Degraded),
            (Self::Degraded, GatewayEvent::Recover) => Ok(Self::Active),
            (Self::Starting | Self::Active | Self::Degraded, GatewayEvent::Fail) => {
                Ok(Self::Failed)
            }
            (Self::Starting | Self::Active | Self::Degraded | Self::Failed, GatewayEvent::Stop) => {
                Ok(Self::Stopping)
            }
            (Self::Stopping, GatewayEvent::FinishStop) => Ok(Self::Stopped),
            (state, rejected) => Err(GatewayTransitionError {
                state,
                event: rejected,
            }),
        }
    }

    /// Return whether captured packets may be admitted.
    pub const fn admits_packets(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Runtime-neutral coordinator for gateway and flow lifecycle state.
///
/// IO, clocks, packet parsing, and platform operations remain outside this type.
pub struct GatewayServer {
    config: GatewayConfig,
    state: GatewayState,
    flows: FlowTable,
    interface: Option<String>,
    exit_availability: ExitAvailability,
    reason: Option<String>,
}

impl GatewayServer {
    /// Construct a stopped server from validated configuration.
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayError> {
        config.validate()?;
        let flows = FlowTable::new(config.max_flows)?;
        Ok(Self {
            config,
            state: GatewayState::Stopped,
            flows,
            interface: None,
            exit_availability: ExitAvailability::Unknown,
            reason: None,
        })
    }

    /// Return current internal lifecycle state.
    pub const fn state(&self) -> GatewayState {
        self.state
    }

    /// Apply a gateway event and fail all flows before cleanup begins.
    pub fn transition(&mut self, event: GatewayEvent) -> Result<GatewayState, GatewayError> {
        let next = self.state.transition(event)?;
        if matches!(next, GatewayState::Stopping | GatewayState::Failed) {
            self.flows.fail_all();
        }
        if next == GatewayState::Stopped {
            self.interface = None;
            self.exit_availability = ExitAvailability::Unknown;
            self.reason = None;
        }
        self.state = next;
        Ok(next)
    }

    /// Record the interface only after platform establishment succeeds.
    pub fn set_established_interface(&mut self, interface: String) {
        self.interface = Some(interface);
    }

    /// Update compatible exit availability and its stable diagnostic.
    pub fn set_exit_availability(
        &mut self,
        availability: ExitAvailability,
        reason: Option<String>,
    ) {
        self.exit_availability = availability;
        self.reason = reason;
    }

    /// Capture a flow only while packet admission is active.
    pub fn capture_flow(&mut self, id: FlowId) -> Result<FlowState, GatewayError> {
        if !self.state.admits_packets() {
            return Err(GatewayError::PacketAdmissionClosed(self.state));
        }
        self.flows.capture(id).map_err(Into::into)
    }

    /// Apply one event to a currently tracked flow.
    pub fn transition_flow(
        &mut self,
        id: FlowId,
        event: FlowEvent,
    ) -> Result<FlowState, GatewayError> {
        self.flows.transition(id, event).map_err(Into::into)
    }

    /// Return the lifecycle state for one currently tracked flow.
    pub fn flow_state(&self, id: FlowId) -> Option<FlowState> {
        self.flows.state(id)
    }

    /// Return the stable process-independent health projection.
    pub fn status(&self) -> GatewayStatus {
        GatewayStatus::from_state(
            self.state,
            self.interface.clone(),
            super::bindings::routes::capture_routes(&self.config.plan),
            self.exit_availability,
            self.flows.len(),
            self.reason.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use ipnet::IpNet;

    use super::*;
    use crate::GatewayHealth;
    use crate::GatewayPlan;
    use crate::Mtu;

    fn config() -> GatewayConfig {
        GatewayConfig {
            plan: GatewayPlan {
                addresses: vec![
                    IpNet::new(Ipv4Addr::new(100, 64, 0, 1).into(), 32).expect("test address")
                ],
                included_routes: vec![
                    IpNet::new(Ipv4Addr::new(198, 18, 0, 0).into(), 15).expect("test route")
                ],
                mtu: Mtu::try_from(1_280).expect("test MTU"),
            },
            max_flows: 2,
            flow_idle_timeout: Duration::from_secs(30),
            tcp_buffer_bytes: 16 * 1_024,
        }
    }

    fn flow() -> FlowId {
        FlowId {
            source: "100.64.0.2:41000".parse().expect("test source"),
            target: "93.184.216.34:443".parse().expect("test target"),
        }
    }

    #[test]
    fn packet_admission_only_follows_successful_start() {
        assert!(!GatewayState::Starting.admits_packets());
        let active = GatewayState::Stopped
            .transition(GatewayEvent::Start)
            .and_then(|state| state.transition(GatewayEvent::AdmitPackets))
            .expect("legal start trace");
        assert!(active.admits_packets());
    }

    #[test]
    fn failure_is_observable_until_cleanup_finishes() {
        let failed = GatewayState::Active
            .transition(GatewayEvent::Fail)
            .expect("active gateway may fail");
        assert_eq!(failed, GatewayState::Failed);
        assert!(!failed.admits_packets());
        let stopped = failed
            .transition(GatewayEvent::Stop)
            .and_then(|state| state.transition(GatewayEvent::FinishStop))
            .expect("failed gateway must remain cleanable");
        assert_eq!(stopped, GatewayState::Stopped);
    }

    #[test]
    fn stopped_gateway_rejects_duplicate_stop() {
        assert_eq!(
            GatewayState::Stopped.transition(GatewayEvent::Stop),
            Err(GatewayTransitionError {
                state: GatewayState::Stopped,
                event: GatewayEvent::Stop,
            })
        );
    }

    #[test]
    fn server_never_admits_a_flow_before_routes_are_ready() {
        let mut server = GatewayServer::new(config()).expect("valid config");
        server.transition(GatewayEvent::Start).expect("start");
        assert!(matches!(
            server.capture_flow(flow()),
            Err(GatewayError::PacketAdmissionClosed(GatewayState::Starting))
        ));
        server
            .transition(GatewayEvent::AdmitPackets)
            .expect("admit packets");
        assert!(matches!(
            server.capture_flow(flow()),
            Ok(FlowState::Captured(id)) if id == flow()
        ));
    }

    #[test]
    fn failure_releases_flows_but_remains_observable() {
        let mut server = GatewayServer::new(config()).expect("valid config");
        server.transition(GatewayEvent::Start).expect("start");
        server.set_established_interface("tun-rings0".to_string());
        server
            .transition(GatewayEvent::AdmitPackets)
            .expect("admit packets");
        server.capture_flow(flow()).expect("active flow");
        server.transition(GatewayEvent::Fail).expect("fail closed");
        let status = server.status();
        assert_eq!(status.health, GatewayHealth::Failed);
        assert_eq!(status.active_flows, 0);
        assert_eq!(status.interface.as_deref(), Some("tun-rings0"));
    }
}
