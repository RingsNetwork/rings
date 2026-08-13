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
    /// Route construction could not select a first hop accepted by the caller.
    NoPermittedFirstHop,
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
    /// Live exits advertise the service transport, but none can serve the requested proxy protocol.
    NoExitForProxyProtocol {
        /// Requested service name.
        service: String,
        /// Requested proxy protocol label.
        protocol: String,
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
    /// A message cannot fit in the largest supported encrypted cell class.
    CellPayloadTooLarge,
    /// A decrypted encrypted cell has an invalid length or internal framing.
    InvalidCell,
    /// A live relay return edge already belongs to another previous hop.
    ReturnEdgeConflict,
    /// The relay return table is full.
    RelayTableFull,
    /// One authenticated previous hop exhausted its share of the relay return table.
    RelayPeerTableFull,
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
    /// A backward sequence number has already delivered a client-side action.
    BackwardReplay,
    /// A circuit direction exhausted its monotonic sequence space.
    SequenceExhausted,
    /// A backward payload carries a return id that does not belong to the local client state.
    BackwardReturnIdMismatch,
    /// A backward payload decoded to a shape that no client adapter may accept.
    UnexpectedBackwardPayload,
    /// The runtime could not allocate a unique circuit id.
    CircuitIdAllocationFailed,
    /// A queued endpoint cell lost its drain task before the overlay reported a result.
    LinkSendCancelled,
    /// A TCP open response channel closed before an answer.
    TcpOpenResponseClosed,
    /// A TCP open request timed out before the exit answered.
    TcpOpenTimedOut,
    /// A TCP stream key is unknown to this runtime.
    UnknownTcpStream,
    /// A TCP stream channel has already closed.
    TcpStreamClosed,
    /// A TCP stream's bounded inbound queue cannot accept another frame.
    TcpStreamBackpressure,
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
            Self::NoPermittedFirstHop => {
                f.write_str("no onion route has a permitted first hop")
            }
            Self::NoLiveExit { service } => {
                write!(f, "no live onion exit offers service {service:?}")
            }
            Self::NoExitWithTransport { service, transport } => write!(
                f,
                "no live onion exit offers service {service:?} over {transport:?}"
            ),
            Self::NoExitForProxyProtocol { service, protocol } => write!(
                f,
                "no live onion exit offers service {service:?} for proxy protocol {protocol:?}"
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
            Self::CellPayloadTooLarge => {
                f.write_str("onion message exceeds the largest encrypted cell class")
            }
            Self::InvalidCell => f.write_str("invalid encrypted onion cell"),
            Self::ReturnEdgeConflict => {
                f.write_str("onion relay return edge already belongs to another previous hop")
            }
            Self::RelayTableFull => f.write_str("onion relay circuit table is full"),
            Self::RelayPeerTableFull => {
                f.write_str("onion relay circuit table quota for previous hop is full")
            }
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
            Self::SequenceExhausted => f.write_str("onion circuit sequence exhausted"),
            Self::BackwardReturnIdMismatch => {
                f.write_str("onion backward payload return id mismatch")
            }
            Self::UnexpectedBackwardPayload => {
                f.write_str("unexpected onion backward payload for client adapter")
            }
            Self::CircuitIdAllocationFailed => {
                f.write_str("failed to allocate unique onion circuit id")
            }
            Self::LinkSendCancelled => {
                f.write_str("onion link send was cancelled before overlay completion")
            }
            Self::TcpOpenResponseClosed => {
                f.write_str("onion TCP open response channel closed")
            }
            Self::TcpOpenTimedOut => f.write_str("onion TCP open timed out"),
            Self::UnknownTcpStream => f.write_str("unknown onion TCP stream"),
            Self::TcpStreamClosed => f.write_str("onion TCP stream is closed"),
            Self::TcpStreamBackpressure => {
                f.write_str("onion TCP stream inbound queue is saturated")
            }
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
            Self::ExitUnavailable => f.write_str("onion exit service is not enabled locally"),
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
    fn wire_internal_failure_does_not_expose_local_diagnostic() {
        let diagnostic = "secret local filesystem and resolver detail";
        let failure = OnionExitFailure::from_error(&Error::InvalidConfig(diagnostic.to_string()));
        let encoded = bincode::serialize(&failure).expect("encode wire failure");

        assert_eq!(failure, OnionExitFailure::Internal);
        assert!(!failure.to_string().contains(diagnostic));
        assert!(!encoded
            .windows(diagnostic.len())
            .any(|window| window == diagnostic.as_bytes()));
    }
}
