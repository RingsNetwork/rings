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
pub struct OnionCircuitCapabilities {
    relay: bool,
    exit_epoch: Option<crate::onion::OnionExitEpoch>,
}

impl OnionCircuitCapabilities {
    /// Build capabilities from the node's advertised relay flag and installed exit epoch.
    pub const fn from_registration(
        relay: bool,
        exit_epoch: Option<crate::onion::OnionExitEpoch>,
    ) -> Self {
        Self { relay, exit_epoch }
    }

    /// Build capabilities for a client-only node.
    pub const fn client() -> Self {
        Self::from_registration(false, None)
    }

    /// Build capabilities for a relay-only node.
    pub const fn relay() -> Self {
        Self::from_registration(true, None)
    }

    /// Build capabilities for an exit-only node in `process_epoch`.
    pub const fn exit(process_epoch: crate::onion::OnionExitEpoch) -> Self {
        Self::from_registration(false, Some(process_epoch))
    }

    pub(super) const fn accepts_forward_layers(self) -> bool {
        self.relay || self.exit_epoch.is_some()
    }

    pub(super) const fn permits_relay_layer(self) -> bool {
        self.relay
    }

    pub(super) fn permits_exit_epoch(self, process_epoch: crate::onion::OnionExitEpoch) -> bool {
        matches!(self.exit_epoch, Some(local_epoch) if local_epoch == process_epoch)
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
