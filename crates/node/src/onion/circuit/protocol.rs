use super::codec::decode_event;
use super::codec::OnionCircuitEvent;
use super::reducer::apply;
use super::reducer::OnionCircuitEffect;
use super::ONION_CIRCUIT_NAMESPACE;
use crate::extension::ext::Ctx;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// The onion data plane as a protocol: a stateless reducer over cells and link facts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OnionCircuitProtocol;

impl Protocol for OnionCircuitProtocol {
    type State = ();
    type Event = OnionCircuitEvent;
    type Effect = OnionCircuitEffect;

    fn namespace(&self) -> &str {
        ONION_CIRCUIT_NAMESPACE
    }

    fn init(&self) -> Self::State {}

    fn decode(&self, wire: Wire<'_>) -> std::result::Result<Self::Event, Reject> {
        decode_event(wire)
    }

    fn step(
        &self,
        _ctx: Ctx<'_, Self::State>,
        event: Self::Event,
    ) -> Transition<Self::State, Self::Effect> {
        apply(event.input)
    }
}
