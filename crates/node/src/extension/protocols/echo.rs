#![warn(missing_docs)]
//! Echo protocol — the reference extension (pure [`Protocol`] + its [`Interpret`] shell).
//!
//! Demonstrates the model end to end: **stateful** (counts the messages seen), **typed**
//! (its own `Event`/`Effect`), and **effectful** (echoes the payload back) — yet `step` is
//! pure and the only IO (an overlay `send`) lives in the interpreter.
//!
//! ```text
//!   S = ℕ
//!   step (Ctx n, Echoed{from, p}) = Transition (n+1) [Reply{to=from, p}]
//! ```

use bytes::Bytes;
use rings_core::dht::Did;

use crate::extension::ext::Ctx;
use crate::extension::ext::Interpret;
use crate::extension::ext::Protocol;
use crate::extension::ext::Reject;
use crate::extension::ext::Scope;
use crate::extension::ext::Transition;
use crate::extension::ext::Wire;

/// Namespace for the echo protocol.
pub const NAMESPACE: &str = "echo";

/// A decoded echo message: who sent it and what bytes to echo back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Echoed {
    /// Sender to echo back to.
    pub from: Did,
    /// Payload to echo.
    pub payload: Bytes,
}

/// Echo's own effect: reply to `to` with `payload` over the overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EchoEffect {
    /// Send `payload` back to `to` under the echo namespace.
    Reply {
        /// Destination.
        to: Did,
        /// Payload to send.
        payload: Bytes,
    },
}

/// Echo protocol: replies with the same payload and counts how many it has seen.
#[derive(Default)]
pub struct Echo;

impl Protocol for Echo {
    /// Number of messages seen so far.
    type State = u64;
    type Event = Echoed;
    type Effect = EchoEffect;

    fn namespace(&self) -> &str {
        NAMESPACE
    }

    fn init(&self) -> u64 {
        0
    }

    fn decode(&self, wire: Wire<'_>) -> Result<Echoed, Reject> {
        Ok(Echoed {
            from: wire.from,
            payload: Bytes::copy_from_slice(wire.payload),
        })
    }

    /// Pure. `step (Ctx n, Echoed{from,p}) = ((n+1), [Reply to=from p])`.
    fn step(&self, ctx: Ctx<'_, u64>, event: Echoed) -> Transition<u64, EchoEffect> {
        Transition::with(ctx.state + 1, vec![EchoEffect::Reply {
            to: event.from,
            payload: event.payload,
        }])
    }
}

/// Echo's interpreter: it owns no resources; a `Reply` is just an overlay `send`.
#[derive(Default)]
pub struct EchoShell;

#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
impl Interpret for EchoShell {
    type Effect = EchoEffect;

    async fn run(&self, scope: &Scope, effect: EchoEffect) -> crate::error::Result<Vec<Bytes>> {
        match effect {
            EchoEffect::Reply { to, payload } => {
                scope.send(to, payload).await?;
                Ok(Vec::new())
            }
        }
    }
}
