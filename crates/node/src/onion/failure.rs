//! Algebraic failure values for local onion routing and exit wire responses.

use std::fmt;

use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Local route/circuit failure before any user-facing rendering.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OnionRouteError {
    /// A pipeline has no symbol application or more than the loop admits (#834 D4a).
    #[error("onion pipeline has {symbols} symbol applications; a loop admits 1 to {max_symbols}")]
    LoopSymbolsOutOfBounds {
        /// Number of symbol applications of the pipeline.
        symbols: usize,
        /// Largest number of symbol applications of one loop.
        max_symbols: usize,
    },
    /// Fewer distinct eligible hops exist than the loop's `H − 1` positions require (#834 D5).
    #[error("onion loop requires {required} distinct hops but only {eligible} are eligible")]
    NotEnoughLoopHops {
        /// Distinct hops the loop requires.
        required: usize,
        /// Distinct eligible hops found.
        eligible: usize,
    },
    /// The symbol registrants admit no assignment of pairwise distinct hops to the symbol
    /// positions of a loop.
    #[error("onion symbol registrants admit no distinct hop per symbol position")]
    NoDistinctSymbolHops,
    /// A loop draw found no candidate although its matching guaranteed one: a violated invariant
    /// of loop selection, never a property of the network.
    #[error("onion loop selection violated its invariant: a guaranteed draw had no candidate")]
    LoopDrawInvariant,
    /// A route's loop does not have the shape of the route's pipeline.
    #[error("onion loop has {actual} symbol hops but its pipeline has {expected}")]
    LoopShapeMismatch {
        /// Symbol applications of the route's pipeline.
        expected: usize,
        /// Symbol hops of the loop.
        actual: usize,
    },
    /// Route construction could not select a first hop accepted by the caller.
    #[error("no onion route has a permitted first hop")]
    NoPermittedFirstHop,
    /// No live exit descriptor offers the requested service.
    #[error("no live onion exit offers service {service:?}")]
    NoLiveExit {
        /// Requested service name.
        service: String,
    },
    /// Every live exit of the service differs from its node's current relay registration in
    /// session key or process epoch: the exit registered from another process, typically before
    /// a restart, and its descriptors have not converged (#834 D2).
    #[error(
        "every onion exit offering service {service:?} differs from its node's current relay \
         registration in session key or process epoch"
    )]
    ExitRelayRegistrationMismatch {
        /// Requested service name.
        service: String,
    },
    /// Every live exit of the service belongs to a node that registers no `relay` (#834 D2).
    #[error("every onion exit offering service {service:?} lacks a relay registration")]
    ExitWithoutRelayRegistration {
        /// Requested service name.
        service: String,
    },
    /// Live exits advertise the service, but none can serve the requested proxy protocol.
    #[error("no live onion exit offers service {service:?} for proxy protocol {protocol:?}")]
    NoExitForProxyProtocol {
        /// Requested service name.
        service: String,
        /// Requested proxy protocol label.
        protocol: String,
    },
    /// Live exits advertise the service, but no policy allows the target.
    #[error("no live onion exit for service {service:?} allows target {target:?}")]
    NoExitAllowsTarget {
        /// Requested service name.
        service: String,
        /// Requested target authority.
        target: String,
    },
    /// Route construction found duplicate DIDs.
    #[error("onion route contains duplicate hops")]
    DuplicateRouteHops,
    /// The selected exit descriptor does not match the final encrypted hop.
    #[error("onion route exit hop does not match exit descriptor")]
    ExitHopMismatch,
    /// The selected exit does not offer the route service.
    #[error("onion route exit does not offer selected service")]
    ExitServiceMismatch,
    /// A payload service does not match its route service.
    #[error(
        "onion payload service {payload_service:?} does not match route service \
         {route_service:?}"
    )]
    PayloadServiceMismatch {
        /// Service label authenticated in the payload.
        payload_service: String,
        /// Service label selected by the route.
        route_service: String,
    },
    /// A message cannot fit in the largest supported encrypted cell class.
    #[error("onion message exceeds the largest encrypted cell class")]
    CellPayloadTooLarge,
    /// A decrypted encrypted cell has an invalid length or internal framing.
    #[error("invalid encrypted onion cell")]
    InvalidCell,
    /// A live relay return edge already belongs to another previous hop.
    #[error("onion relay return edge already belongs to another previous hop")]
    ReturnEdgeConflict,
    /// The relay return table is full.
    #[error("onion relay circuit table is full")]
    RelayTableFull,
    /// One authenticated previous hop exhausted its share of the relay return table.
    #[error("onion relay circuit table quota for previous hop is full")]
    RelayPeerTableFull,
    /// A backward payload signer is not the selected exit DID.
    #[error("onion backward payload signer is not the selected exit")]
    BackwardSignerMismatch,
    /// A backward payload signer account key is not the selected exit key.
    #[error("onion backward payload account key is not the selected exit")]
    BackwardAccountKeyMismatch,
    /// A backward payload delegatee key is not the selected exit delegatee key.
    #[error("onion backward payload delegatee key is not the selected exit")]
    BackwardSessionKeyMismatch,
    /// A backward payload signature or freshness proof is invalid.
    #[error("invalid onion backward payload signature")]
    InvalidBackwardSignature,
    /// A forward nonce has already authorized an exit-side action.
    #[error("replayed onion forward payload")]
    ForwardReplay,
    /// A forward payload reached the exit after its authenticated expiry.
    #[error("expired onion forward payload")]
    ForwardPayloadExpired,
    /// A forward payload names an exit process epoch that is no longer active.
    #[error("onion forward payload belongs to another exit process epoch")]
    ForwardEpochMismatch,
    /// A backward sequence number has already delivered a client-side action.
    #[error("replayed onion TCP backward payload")]
    BackwardReplay,
    /// A circuit direction exhausted its monotonic sequence space.
    #[error("onion circuit sequence exhausted")]
    SequenceExhausted,
    /// A backward payload carries a return id that does not belong to the local client state.
    #[error("onion backward payload return id mismatch")]
    BackwardReturnIdMismatch,
    /// A backward payload decoded to a shape that no client adapter may accept.
    #[error("unexpected onion backward payload for client adapter")]
    UnexpectedBackwardPayload,
    /// The runtime could not allocate a unique circuit id.
    #[error("failed to allocate unique onion circuit id")]
    CircuitIdAllocationFailed,
    /// A queued endpoint cell lost its drain task before the overlay reported a result.
    #[error("onion link send was cancelled before overlay completion")]
    LinkSendCancelled,
    /// An HTTPS response channel closed before the exit's outcome was delivered.
    #[error("onion HTTPS response channel closed")]
    HttpsResponseClosed,
    /// A TCP open response channel closed before an answer.
    #[error("onion TCP open response channel closed")]
    TcpOpenResponseClosed,
    /// A TCP open request timed out before the exit answered.
    #[error("onion TCP open timed out")]
    TcpOpenTimedOut,
    /// A TCP stream key is unknown to this runtime.
    #[error("unknown onion TCP stream")]
    UnknownTcpStream,
    /// A TCP stream channel has already closed.
    #[error("onion TCP stream is closed")]
    TcpStreamClosed,
    /// A TCP stream's bounded inbound queue cannot accept another frame.
    #[error("onion TCP stream inbound queue is saturated")]
    TcpStreamBackpressure,
    /// A duplicate TCP open targeted a live circuit.
    #[error("duplicate onion TCP open for live circuit")]
    DuplicateTcpOpen,
    /// A received TCP return peer differs from the selected route peer.
    #[error("unexpected onion TCP return peer: expected {expected:?}, got {actual:?}")]
    UnexpectedTcpReturnPeer {
        /// Return peer selected by the client route.
        expected: Did,
        /// Peer that delivered the backward payload.
        actual: Did,
    },
    /// A received TCP forward peer differs from the selected route peer.
    #[error("unexpected onion TCP forward peer: expected {expected:?}, got {actual:?}")]
    UnexpectedTcpForwardPeer {
        /// Forward peer recorded when the exit accepted the circuit.
        expected: Did,
        /// Peer that delivered the forward payload.
        actual: Did,
    },
    /// An exit-reported failure reached the local route client.
    #[error("{0}")]
    ExitFailure(OnionExitFailure),
    /// A test-only route fixture was missing an expected relay.
    #[cfg(test)]
    #[error("missing test relay")]
    MissingTestRelay,
}

/// Recoverable failure reported by an onion exit to its client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnionExitFailure {
    /// The exit policy or local limiter denied the operation.
    PermissionDenied,
    /// The target name could not be resolved.
    ResolveTarget,
    /// The exit could not connect to the target.
    ConnectTarget,
    /// The exit failed while reading from the target.
    ReadTarget,
    /// The exit rejected a replayed payload.
    Replay,
    /// The client supplied a malformed target for this exit protocol.
    InvalidTarget(String),
    /// The exit rejected a duplicate live circuit.
    DuplicateCircuit,
    /// The exit hit a local internal failure while answering the request.
    Internal,
}

impl OnionExitFailure {
    /// Convert a local node error into a wire failure at the adapter boundary.
    pub fn from_error(error: &Error) -> Self {
        match error {
            Error::NoPermission => Self::PermissionDenied,
            Error::OnionRouteError(OnionRouteError::ForwardReplay)
            | Error::OnionRouteError(OnionRouteError::ForwardPayloadExpired)
            | Error::OnionRouteError(OnionRouteError::BackwardReplay) => Self::Replay,
            Error::OnionRouteError(OnionRouteError::DuplicateTcpOpen) => Self::DuplicateCircuit,
            _ => Self::Internal,
        }
    }
}

impl fmt::Display for OnionExitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => Error::NoPermission.fmt(f),
            Self::ResolveTarget => f.write_str("onion exit could not resolve target"),
            Self::ConnectTarget => f.write_str("onion exit could not connect to target"),
            Self::ReadTarget => f.write_str("onion exit could not read target"),
            Self::InvalidTarget(message) => f.write_str(message),
            Self::Replay => f.write_str("replayed onion payload"),
            Self::DuplicateCircuit => f.write_str("duplicate onion TCP open for live circuit"),
            Self::Internal => f.write_str("onion exit internal failure"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnionExitFailure;
    use crate::error::Error;

    #[test]
    fn test_wire_internal_failure_does_not_expose_local_diagnostic() {
        let diagnostic = "secret local filesystem and resolver detail";
        let failure = OnionExitFailure::from_error(&Error::InvalidConfig(diagnostic.to_string()));
        let encoded = rings_codec::serialize(&failure).expect("encode wire failure");

        assert_eq!(failure, OnionExitFailure::Internal);
        assert!(!failure.to_string().contains(diagnostic));
        assert!(!encoded
            .windows(diagnostic.len())
            .any(|window| window == diagnostic.as_bytes()));
    }
}
