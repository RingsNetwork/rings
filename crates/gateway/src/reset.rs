//! The reset law: how the runtime names the end of a captured flow (#950).
//!
//! # Model
//!
//! The runtime names a flow's end at one instant, from three observations:
//!
//! ```text
//! φ ∈ FlowPhase      = Handshake | Opening | Open(σ)       what the flow was waiting on
//!     Handshake        the TCP endpoint has not established          (flow TargetBound)
//!     Opening          the bridge awaits open_stream                 (flow Opening)
//!     Open(σ)          the Onion stream opened                       (Established, HalfClosed)
//! σ ∈ Condition      = Live | Stalled | Failed(c)          the stream probe, read at the instant
//! κ ∈ EndpointClosure = ClientReset | ClientSilent | Ungraceful   how the TCP endpoint closed
//!     ClientReset      a client RST closed it
//!     ClientSilent     smoltcp's idle clock closed it: no client segment for T (now ≥ c + T)
//!     Ungraceful       a FIN exchange closed it before both directions ended at the bridge
//! ```
//!
//! The captured-flow path, as transitions of `(φ, σ)` and the runtime's answer to each:
//!
//! ```text
//! capture(SYN)        φ ← Handshake
//! establish           φ ← Opening                    (the bridge spawns; open_stream starts:
//!                                                     route build, first T loop, open ack)
//! open ok(probe)      φ ← Open(Live)                 open err(e) ⇒ reset bridge(φ, e)
//! reply out of order  σ: Live → Stalled              (a later frame is held behind a hole)
//! hole filled         σ: Stalled → Live
//! circuit fails(c)    σ ← Failed(c)                  a gap persisting V, silence Δ = 2V, an
//!                                                     abort, a link fact, a failed pump; read
//!                                                     before the duplex ends (report before end)
//! data                σ unchanged; the client segment or the stream byte restarts the clock
//! EOF                 eof(σ):  ⊥ ⇒ half-close;  StreamReset(c) ⇒ reset
//! bridge IO error(e)  reset bridge(φ, e)
//! endpoint closed(κ)  reset closure(φ, κ)            (the gateway idle clock is κ = ClientSilent)
//! ```
//!
//! # Classifier
//!
//! ```text
//! failure(Open(Failed(c))) = StreamReset(c);    failure(φ) = ⊥ otherwise
//!
//! closure(φ, κ) = failure(φ) ?: local(φ, κ)
//!   local(φ, ClientReset)               = ClientReset
//!   local(φ, Ungraceful)                = UngracefulClose
//!   local(Handshake, ClientSilent)      = HandshakeExpired
//!   local(Opening, ClientSilent)        = OpenExpired
//!   local(Open(Stalled), ClientSilent)  = StreamStalled
//!   local(Open(Live), ClientSilent)     = IdleTimeout
//! bridge(φ, e)  = failure(φ) ?: of_error(e)
//! eof(σ)        = failure(Open(σ))
//! ```
//!
//! where `a ?: b` is `a` when defined and `b` otherwise ([`Option::unwrap_or_else`]).
//!
//! # Laws
//!
//! - **Idle.** `closure(φ, κ) = IdleTimeout ⇔ κ = ClientSilent ∧ φ = Open(Live)`: a flow reset
//!   is an idle timeout iff the client was silent for `T` while its stream was open, no failure
//!   of the stream's circuit had been observed, and no frame of it was known missing.
//! - **Dominance.** `σ = Failed(c) ⇒ closure(Open(σ), κ) = bridge(Open(σ), e) = eof(σ) =
//!   StreamReset(c)` for every `κ` and `e`: an observed circuit failure is reported as itself,
//!   whichever local event reaches the runtime first.
//! - **Phase.** An expired clock names what was waiting: `Handshake ↦ HandshakeExpired`,
//!   `Opening ↦ OpenExpired`, `Open(Stalled) ↦ StreamStalled`; none of them is `IdleTimeout`.
//! - **Totality.** `closure` and `bridge` are total, so every reset carries a reason.
//!
//! Proof. `failure` is defined only on `Open(Failed(c))`, so on that phase every classifier
//! returns `StreamReset(c)` (Dominance). Elsewhere `closure = local`, whose cases restricted to
//! the phases without a failure name `ClientSilent` by phase, and only `(Open(Live),
//! ClientSilent)` maps to `IdleTimeout` (Idle, Phase). `local` and `of_error` are total, hence
//! so is every classifier (Totality). The domain is finite, and `reset::tests` checks each law
//! on all of it.
//!
//! # Residual: losses no endpoint observes
//!
//! `σ` is a function of what the client endpoint has observed, and a lost frame is observed
//! only through a later frame of its direction. Two losses leave every observation unchanged:
//!
//! - a lost last forward `data`: `h` never sees it (credit carries no sequence) while it keeps
//!   echoing the keep-alive, so neither endpoint ever fails the session;
//! - a lost reply tail that includes the world's `fin`: `h` sends no echo after `fin`, so only
//!   the session's own deadline `Δ = 2V` observes it.
//!
//! A lost reply before `fin` is not among them: the next sequenced reply (a later `data`, the
//! `fin`, or the echo `data(n, 0, ε)`) exposes it, `σ` reads `Stalled` (`StreamStalled`), and the
//! gap rule fails the session after `V`. Until an unobserved loss is observed, the run is
//! indistinguishable, at the gateway and at the client, from a healthy flow whose target is
//! silent, so `closure`, a function of the observations, names both `IdleTimeout`. Making such
//! a loss observable is a change to the loop protocol (#981).

use crate::FlowResetReason;
use crate::GatewayError;
use crate::OnionStreamCondition;

/// What a captured flow was waiting on when its end was named (`φ` in the module model).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FlowPhase {
    /// The TCP endpoint had not established.
    Handshake,
    /// The bridge was opening the Onion stream.
    Opening,
    /// The Onion stream was open, with its probe's reading at the instant.
    Open(OnionStreamCondition),
}

/// How a TCP endpoint closed before both byte directions shut down (`κ` in the module model),
/// from facts the stack recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EndpointClosure {
    /// A client RST closed the endpoint.
    ClientReset,
    /// smoltcp's idle clock closed the endpoint: no client segment arrived for the flow idle
    /// timeout.
    ClientSilent,
    /// A FIN exchange closed the endpoint before both directions ended at the bridge.
    Ungraceful,
}

/// `failure(φ)`: the observed circuit failure of a flow in `phase`, if any, which dominates every
/// local cause (Law Dominance).
fn failure(phase: &FlowPhase) -> Option<FlowResetReason> {
    match phase {
        FlowPhase::Open(OnionStreamCondition::Failed(cause)) => {
            Some(FlowResetReason::StreamReset(cause.clone()))
        }
        FlowPhase::Handshake | FlowPhase::Opening | FlowPhase::Open(_) => None,
    }
}

/// `closure(φ, κ) = failure(φ) ?: local(φ, κ)`: the reason of a flow in `phase` whose TCP
/// endpoint closed by `closure`.
pub(crate) fn closure_reason(phase: &FlowPhase, closure: EndpointClosure) -> FlowResetReason {
    failure(phase).unwrap_or_else(|| local(phase, closure))
}

/// `local(φ, κ)`: the name of the local cause `closure` of a flow in `phase` alone. It is total;
/// on a failed stream it is never the result, since `failure` dominates it.
fn local(phase: &FlowPhase, closure: EndpointClosure) -> FlowResetReason {
    match (phase, closure) {
        (_, EndpointClosure::ClientReset) => FlowResetReason::ClientReset,
        (_, EndpointClosure::Ungraceful) => FlowResetReason::UngracefulClose,
        (FlowPhase::Handshake, EndpointClosure::ClientSilent) => FlowResetReason::HandshakeExpired,
        (FlowPhase::Opening, EndpointClosure::ClientSilent) => FlowResetReason::OpenExpired,
        (FlowPhase::Open(OnionStreamCondition::Stalled), EndpointClosure::ClientSilent) => {
            FlowResetReason::StreamStalled
        }
        (
            FlowPhase::Open(OnionStreamCondition::Live | OnionStreamCondition::Failed(_)),
            EndpointClosure::ClientSilent,
        ) => FlowResetReason::IdleTimeout,
    }
}

/// `bridge(φ, e)`: the reason of a flow in `phase` whose byte bridge failed with `error`.
///
/// Stream errors keep their operation label; every other bridge failure happened while the
/// connector was opening the stream. Flow addresses are not copied into the reason.
pub(crate) fn bridge_reason(phase: &FlowPhase, error: &GatewayError) -> FlowResetReason {
    failure(phase).unwrap_or_else(|| match error {
        GatewayError::StreamIo {
            operation, message, ..
        } => FlowResetReason::StreamFailed(format!("{operation}: {message}")),
        GatewayError::OnionUnavailable { message, .. } => {
            FlowResetReason::OnionOpenFailed(message.clone())
        }
        other => FlowResetReason::OnionOpenFailed(other.to_string()),
    })
}

/// `eof(σ)`: the reset an EOF of a stream reading `condition` stands for, or `None` for an
/// orderly end, which half-closes the flow.
pub(crate) fn eof_reason(condition: OnionStreamCondition) -> Option<FlowResetReason> {
    failure(&FlowPhase::Open(condition))
}

#[cfg(test)]
mod tests;
