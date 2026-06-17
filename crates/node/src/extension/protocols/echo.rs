#![warn(missing_docs)]
//! Echo protocol — the reference [`Protocol`] implementation.
//!
//! Demonstrates the pure model end to end: it is **stateful** (counts the messages
//! seen) and **effectful** (echoes the payload back), yet `step` performs no IO.
//!
//! ```text
//!   S = ℕ
//!   step (Ctx n, Event{from, p}) = Transition (n+1) [Send{to=from, ns="echo", p}]
//! ```

use crate::extension::ext::Ctx;
use crate::extension::ext::Effect;
use crate::extension::ext::Event;
use crate::extension::ext::Protocol;
use crate::extension::ext::Transition;

/// Namespace for the echo protocol.
pub const NAMESPACE: &str = "echo";

/// Echo protocol: replies with the same payload and counts how many it has seen.
#[derive(Default)]
pub struct Echo;

impl Protocol for Echo {
    /// Number of messages seen so far.
    type State = u64;

    fn namespace(&self) -> &str {
        NAMESPACE
    }

    fn init(&self) -> u64 {
        0
    }

    /// Pure. `step (Ctx n, Event{from,p}) = ((n+1), [Send to=from ns="echo" p])`.
    fn step(&self, ctx: Ctx<'_, u64>, event: &Event) -> Transition<u64> {
        Transition::with(ctx.state + 1, vec![Effect::Send {
            to: event.from,
            namespace: NAMESPACE.to_string(),
            payload: event.payload.clone(),
        }])
    }
}
