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
///
/// `relay` and `exit` are distinct application-layer roles. Both may receive encrypted forward
/// frames, but only relay nodes may forward a relay layer to a next hop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnionCircuitCapabilities {
    relay: bool,
    exit: bool,
}

impl OnionCircuitCapabilities {
    /// Build capabilities from the node's advertised relay flag and installed exit service.
    pub const fn from_registration(relay: bool, exit: bool) -> Self {
        Self { relay, exit }
    }

    /// Build capabilities for a client-only node.
    pub const fn client() -> Self {
        Self {
            relay: false,
            exit: false,
        }
    }

    pub(super) const fn accepts_forward_layers(self) -> bool {
        self.relay || self.exit
    }

    pub(super) const fn permits_relay_layer(self) -> bool {
        self.relay
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
        self.reducer.apply(ctx.did, ctx.state, event.input)
    }
}
