use super::codec::decode_event;
use super::codec::OnionCircuitEvent;
use super::reducer::OnionCircuitEffect;
use super::reducer::OnionCircuitReducer;
use super::reducer::OnionCircuitState;
use super::ONION_CIRCUIT_NAMESPACE;
use crate::extension::ext::Ctx;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// Capabilities this node enables for the onion circuit data plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionCircuitCapabilities {
    /// This node only creates client circuits.
    Client,
    /// This node forwards relay layers but does not execute exit payloads.
    Relay,
    /// This node executes exit payloads but does not forward relay layers.
    Exit,
    /// This node can both relay and execute exit payloads.
    RelayAndExit,
}

impl OnionCircuitCapabilities {
    /// Build capabilities from the node's advertised relay flag and installed exit service.
    pub const fn from_registration(relay: bool, exit: bool) -> Self {
        match (relay, exit) {
            (false, false) => Self::Client,
            (true, false) => Self::Relay,
            (false, true) => Self::Exit,
            (true, true) => Self::RelayAndExit,
        }
    }

    /// Build capabilities for a client-only node.
    pub const fn client() -> Self {
        Self::Client
    }

    /// Build capabilities for a relay-only node.
    pub const fn relay() -> Self {
        Self::Relay
    }

    /// Build capabilities for an exit-only node.
    pub const fn exit() -> Self {
        Self::Exit
    }

    pub(super) const fn accepts_forward_layers(self) -> bool {
        matches!(self, Self::Relay | Self::Exit | Self::RelayAndExit)
    }

    pub(super) const fn permits_relay_layer(self) -> bool {
        matches!(self, Self::Relay | Self::RelayAndExit)
    }

    pub(super) const fn permits_exit_layer(self) -> bool {
        matches!(self, Self::Exit | Self::RelayAndExit)
    }
}

/// Encrypted onion circuit protocol.
#[derive(Clone, Debug)]
pub struct OnionCircuitProtocol {
    reducer: OnionCircuitReducer,
}

impl OnionCircuitProtocol {
    /// Create a protocol instance over explicit onion circuit capabilities.
    pub fn new(capabilities: OnionCircuitCapabilities) -> Self {
        Self {
            reducer: OnionCircuitReducer::new(capabilities),
        }
    }
}

impl Protocol for OnionCircuitProtocol {
    type State = OnionCircuitState;
    type Event = OnionCircuitEvent;
    type Effect = OnionCircuitEffect;

    fn namespace(&self) -> &str {
        ONION_CIRCUIT_NAMESPACE
    }

    fn init(&self) -> Self::State {
        OnionCircuitState::default()
    }

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        decode_event(wire)
    }

    fn step(
        &self,
        ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect> {
        self.reducer.apply(ctx.state, event.input)
    }
}
