//! The data plane as a protocol: stateless, and each input is its own one effect.
//!
//! ```text
//! step(Cell(from, cell)) = [Cell(from, cell)]      step(Link(fact)) = [Link(fact)]
//! ```
//!
//! Relays keep no state (#834 Phase 2a item 2), and the admission state, the only state of the
//! data plane, is owned by the shell, which applies the pure hop step to it in the order `step`
//! emits these effects: the protocol's transition gate linearises cells and link facts into one
//! stream, which is the linearisation obligation of admission's `reconcile`.

use super::codec::decode_event;
use super::codec::OnionCircuitInput;
use super::ONION_CIRCUIT_NAMESPACE;
use crate::extension::ext::Ctx;
use crate::extension::ext::LinkFact;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// The onion data plane as a protocol: the identity transition over cells and link facts.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OnionCircuitProtocol;

impl Protocol for OnionCircuitProtocol {
    type State = ();
    type Event = OnionCircuitInput;
    type Effect = OnionCircuitInput;

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
        Transition::with((), vec![event])
    }

    /// The onion data plane admits its own direct-edge traffic (#888): every cell is charged
    /// `u(b) = b / 16 KiB` against the live link it arrived on, on receipt and before any
    /// cryptography (#834 L9, see `circuit::admission`), so a sending neighbour is bounded by its
    /// link's unit budget. Core's per-origin message count would only throttle a link's
    /// constant-rate emission below `r`; the per-origin byte bucket still applies.
    fn delegates_admission(&self) -> bool {
        true
    }

    /// Every fact about a link reaches the data plane, in its queue's order: an admission makes
    /// the link live, a retirement ends it, and both a retirement and a drop end the sessions
    /// this node holds on it (`Law (Circuit fate)`, #963, #966, #969; see `circuit::shell`).
    fn link_fact(&self, fact: LinkFact) -> Option<Self::Event> {
        Some(OnionCircuitInput::Link(fact.into()))
    }
}
