//! The data plane's reducer: stateless, one effect per input.
//!
//! ```text
//! Cell(from, cell)   ↦  [Hop(from, cell)]
//! Link(fact)         ↦  [Link(fact)]
//! ```
//!
//! Relays keep no state (#834 Phase 2a item 2), and the admission state, the only state of the
//! data plane, is owned by the shell, which applies the pure hop step to it in the order the
//! reducer emits these effects: the protocol's transition gate linearises cells and link facts
//! into one stream, which is the linearisation obligation of admission's `reconcile`.

use rings_core::dht::Did;

use super::codec::OnionCircuitInput;
use super::codec::OnionLinkFact;
use crate::extension::ext::Transition;
use crate::onion::sphinx::cell::OnionCell;

/// Effects of the data plane, performed by the shell.
#[derive(Debug)]
pub(crate) enum OnionCircuitEffect {
    /// Run `Hop_i` on a received cell.
    Hop {
        /// The authenticated previous hop.
        from: Did,
        /// The cell.
        cell: OnionCell,
    },
    /// Apply a link fact to the admission state.
    Link(OnionLinkFact),
}

/// The reducer's transition: `()` to `()`, with the input's one effect.
pub(super) fn apply(input: OnionCircuitInput) -> Transition<(), OnionCircuitEffect> {
    let effect = match input {
        OnionCircuitInput::Cell { from, cell } => OnionCircuitEffect::Hop { from, cell },
        OnionCircuitInput::Link(fact) => OnionCircuitEffect::Link(fact),
    };
    Transition::with((), vec![effect])
}
