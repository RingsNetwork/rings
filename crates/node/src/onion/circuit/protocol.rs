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
use crate::onion::OnionProcessEpoch;
use crate::onion::OnionRole;

/// Capabilities this node enables for the onion circuit data plane: its [`OnionRole`] at the
/// process epoch `e_n` exit layers must name, the image of the configured role under
/// `OnionRole::map(|_| e_n)` (#834 D2).
///
/// A [`OnionRole::Client`] accepts no forward layer; every other rung relays; only
/// [`OnionRole::Exit`] evaluates an exit layer, and only one sealed for its own process.
pub type OnionCircuitCapabilities = OnionRole<OnionProcessEpoch>;

impl OnionCircuitCapabilities {
    /// Return whether an exit layer sealed for `process_epoch` may be evaluated here.
    pub(super) fn permits_exit_layer(self, process_epoch: OnionProcessEpoch) -> bool {
        matches!(self, Self::Exit(local) if local == process_epoch)
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
