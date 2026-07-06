//! Algebraic failure values for local onion routing and exit wire responses.

use std::fmt;

use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use super::OnionExitTransport;
use crate::error::Error;

/// Local route/circuit failure before any user-facing rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OnionRouteError {
    /// A route or circuit was unexpectedly empty.
    RouteHasNoHops,
    /// The requested route service is empty after normalization.
    EmptyRouteService,
    /// The requested or constructed hop count is outside the circuit bound.
    HopCountOutOfBounds {
        /// Requested or constructed hop count.
        hop_count: usize,
        /// Maximum hop count accepted by this circuit implementation.
        max_hops: u8,
    },
    /// Route construction could not select enough relay hops.
    NotEnoughRelays {
        /// Requested hop count including the exit.
        hop_count: usize,
    },
    /// No live exit descriptor offers the requested service.
    NoLiveExit {
        /// Requested service name.
        service: String,
    },
    /// Live exits advertise the service name but none use the required transport.
    NoExitWithTransport {
        /// Requested service name.
        service: String,
        /// Required transport class.
        transport: OnionExitTransport,
    },
    /// Live exits advertise the service and transport, but no policy allows the target.
    NoExitAllowsTarget {
        /// Requested service name.
        service: String,
        /// Requested target authority.
        target: String,
    },
    /// Route construction found duplicate DIDs.
    DuplicateRouteHops,
    /// The selected exit descriptor does not match the final encrypted hop.
    ExitHopMismatch,
    /// The selected exit does not offer the route service.
    ExitServiceMismatch,
    /// A payload service does not match its route service.
    PayloadServiceMismatch {
        /// Service label authenticated in the payload.
        payload_service: String,
        /// Service label selected by the route.
        route_service: String,
    },
    /// A relay layer references a missing next hop.
    MissingNextHop,
    /// A constructed circuit path does not have exactly one edge id per hop.
    CircuitPathLengthMismatch {
        /// Number of encrypted hops in the route.
        hop_count: usize,
        /// Number of edge ids carried by the circuit path.
        edge_count: usize,
    },
    /// A relay layer carries an invalid hop budget.
    InvalidRelayHopBudget {
        /// Remaining encrypted hops claimed by the relay layer.
        remaining_hops: u8,
        /// Maximum relay hop budget accepted by this node.
        max_hops: u8,
    },
    /// A live relay return edge already belongs to another previous hop.
    ReturnEdgeConflict,
    /// The relay return table is full.
    RelayTableFull,
    /// A backward payload signer is not the selected exit DID.
    BackwardSignerMismatch,
    /// A backward payload signer account key is not the selected exit key.
    BackwardAccountKeyMismatch,
    /// A backward payload session key is not the selected exit session key.
    BackwardSessionKeyMismatch,
    /// A backward payload signature or freshness proof is invalid.
    InvalidBackwardSignature,
    /// A forward nonce has already authorized an exit-side action.
    ForwardReplay,
    /// A forward payload reached the exit after its authenticated expiry.
    ForwardPayloadExpired,
    /// A backward nonce has already delivered a client-side action.
    BackwardReplay,
    /// A backward payload carries a return id that does not belong to the local client state.
    BackwardReturnIdMismatch,
    /// A backward payload decoded to a shape that no client adapter may accept.
    UnexpectedBackwardPayload,
    /// The runtime could not allocate a unique circuit id.
    CircuitIdAllocationFailed,
    /// A TCP open response channel closed before an answer.
    TcpOpenResponseClosed,
    /// A TCP open request timed out before the exit answered.
    TcpOpenTimedOut,
    /// A TCP stream key is unknown to this runtime.
    UnknownTcpStream,
    /// A TCP stream channel has already closed.
    TcpStreamClosed,
    /// A duplicate TCP open targeted a live circuit.
    DuplicateTcpOpen,
    /// A received TCP return peer differs from the selected route peer.
    UnexpectedTcpReturnPeer {
        /// Return peer selected by the client route.
        expected: Did,
        /// Peer that delivered the backward payload.
        actual: Did,
    },
    /// A received TCP forward peer differs from the selected route peer.
    UnexpectedTcpForwardPeer {
        /// Forward peer recorded when the exit accepted the circuit.
        expected: Did,
        /// Peer that delivered the forward payload.
        actual: Did,
    },
    /// An exit-reported failure reached the local route client.
    ExitFailure(OnionExitFailure),
    /// A test-only route fixture was missing an expected relay.
    #[cfg(test)]
    MissingTestRelay,
}

impl fmt::Display for OnionRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RouteHasNoHops => f.write_str("onion route has no hops"),
            Self::EmptyRouteService => f.write_str("onion route service must not be empty"),
            Self::HopCountOutOfBounds {
                hop_count,
                max_hops,
            } => write!(f, "onion route hop count {hop_count} exceeds limit {max_hops}"),
            Self::NotEnoughRelays { hop_count } => {
                write!(f, "not enough relay candidates for {hop_count}-hop onion route")
            }
            Self::NoLiveExit { service } => {
                write!(f, "no live onion exit offers service {service:?}")
            }
            Self::NoExitWithTransport { service, transport } => write!(
                f,
                "no live onion exit offers service {service:?} over {transport:?}"
            ),
            Self::NoExitAllowsTarget { service, target } => write!(
                f,
                "no live onion exit for service {service:?} allows target {target:?}"
            ),
            Self::DuplicateRouteHops => f.write_str("onion route contains duplicate hops"),
            Self::ExitHopMismatch => {
                f.write_str("onion route exit hop does not match exit descriptor")
            }
            Self::ExitServiceMismatch => {
                f.write_str("onion route exit does not offer selected service")
            }
            Self::PayloadServiceMismatch {
                payload_service,
                route_service,
            } => write!(
                f,
                "onion payload service {payload_service:?} does not match route service {route_service:?}"
            ),
            Self::MissingNextHop => f.write_str("missing next onion hop"),
            Self::CircuitPathLengthMismatch {
                hop_count,
                edge_count,
            } => write!(
                f,
                "onion circuit path has {edge_count} edge ids for {hop_count} route hops"
            ),
            Self::InvalidRelayHopBudget {
                remaining_hops,
                max_hops,
            } => write!(
                f,
                "invalid onion relay hop budget {remaining_hops}; maximum is {max_hops}"
            ),
            Self::ReturnEdgeConflict => {
                f.write_str("onion relay return edge already belongs to another previous hop")
            }
            Self::RelayTableFull => f.write_str("onion relay circuit table is full"),
            Self::BackwardSignerMismatch => {
                f.write_str("onion backward payload signer is not the selected exit")
            }
            Self::BackwardAccountKeyMismatch => {
                f.write_str("onion backward payload account key is not the selected exit")
            }
            Self::BackwardSessionKeyMismatch => {
                f.write_str("onion backward payload session key is not the selected exit")
            }
            Self::InvalidBackwardSignature => {
                f.write_str("invalid onion backward payload signature")
            }
            Self::ForwardReplay => f.write_str("replayed onion forward payload"),
            Self::ForwardPayloadExpired => f.write_str("expired onion forward payload"),
            Self::BackwardReplay => f.write_str("replayed onion TCP backward payload"),
            Self::BackwardReturnIdMismatch => {
                f.write_str("onion backward payload return id mismatch")
            }
            Self::UnexpectedBackwardPayload => {
                f.write_str("unexpected onion backward payload for client adapter")
            }
            Self::CircuitIdAllocationFailed => {
                f.write_str("failed to allocate unique onion circuit id")
            }
            Self::TcpOpenResponseClosed => {
                f.write_str("onion TCP open response channel closed")
            }
            Self::TcpOpenTimedOut => f.write_str("onion TCP open timed out"),
            Self::UnknownTcpStream => f.write_str("unknown onion TCP stream"),
            Self::TcpStreamClosed => f.write_str("onion TCP stream is closed"),
            Self::DuplicateTcpOpen => f.write_str("duplicate onion TCP open for live circuit"),
            Self::UnexpectedTcpReturnPeer { expected, actual } => write!(
                f,
                "unexpected onion TCP return peer: expected {expected:?}, got {actual:?}"
            ),
            Self::UnexpectedTcpForwardPeer { expected, actual } => write!(
                f,
                "unexpected onion TCP forward peer: expected {expected:?}, got {actual:?}"
            ),
            Self::ExitFailure(failure) => failure.fmt(f),
            #[cfg(test)]
            Self::MissingTestRelay => f.write_str("missing test relay"),
        }
    }
}

/// Recoverable failure reported by an onion exit to its client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnionExitFailure {
    /// The requested exit service is not enabled on the selected node.
    ExitUnavailable,
    /// The exit policy or local limiter denied the operation.
    PermissionDenied,
    /// The target name could not be resolved.
    ResolveTarget(String),
    /// The exit could not connect to the target.
    ConnectTarget(String),
    /// The exit failed while reading from the target.
    ReadTarget(String),
    /// The exit rejected a replayed payload.
    Replay,
    /// The client supplied a malformed target for this exit protocol.
    InvalidTarget(String),
    /// The exit rejected a duplicate live circuit.
    DuplicateCircuit,
    /// The exit hit a local internal failure while answering the request.
    Internal(String),
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
            _ => Self::Internal(error.to_string()),
        }
    }
}

impl fmt::Display for OnionExitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitUnavailable => f.write_str("onion exit service is not enabled locally"),
            Self::PermissionDenied => Error::NoPermission.fmt(f),
            Self::ResolveTarget(message)
            | Self::ConnectTarget(message)
            | Self::ReadTarget(message)
            | Self::InvalidTarget(message)
            | Self::Internal(message) => f.write_str(message),
            Self::Replay => f.write_str("replayed onion payload"),
            Self::DuplicateCircuit => f.write_str("duplicate onion TCP open for live circuit"),
        }
    }
}
