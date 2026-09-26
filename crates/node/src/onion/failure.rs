//! Algebraic failure values for local onion routing and exit wire responses.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Local route and session failure before any user-facing rendering.
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
    /// The selected exit descriptor does not match the loop's symbol hop.
    #[error("onion route exit hop does not match exit descriptor")]
    ExitHopMismatch,
    /// The selected exit does not offer the route service.
    #[error("onion route exit does not offer selected service")]
    ExitServiceMismatch,
    /// A session's symbol does not match the service its route was selected for.
    #[error(
        "onion payload service {payload_service:?} does not match route service \
         {route_service:?}"
    )]
    PayloadServiceMismatch {
        /// Symbol the session applies.
        payload_service: String,
        /// Service label selected by the route.
        route_service: String,
    },
    /// A cell's length is the length of no loop class.
    #[error("invalid onion cell")]
    InvalidCell,
    /// A queued endpoint cell lost its drain task before the overlay reported a result.
    #[error("onion link send was cancelled before overlay completion")]
    LinkSendCancelled,
    /// An HTTPS response channel closed before the exit's outcome was delivered.
    #[error("onion HTTPS response channel closed")]
    HttpsResponseClosed,
    /// The exit's connect to a `tcp` target did not complete within its open timeout.
    #[error("onion TCP open timed out")]
    TcpOpenTimedOut,
    /// A session's driver has already ended.
    #[error("onion TCP stream is closed")]
    TcpStreamClosed,
    /// The client could not build a loop over its route (a value too wide for its class, or a
    /// key failure of negligible probability).
    #[error("could not build an onion loop: {0}")]
    LoopBuild(String),
    /// The exit refused to open the session, or the session ended before it opened; the exit
    /// gives no reason (#834 D2′).
    #[error("the onion exit refused the session")]
    ExitRefused,
    /// A session failed closed: a gap in its sequence, or its loops could not leave.
    #[error("the onion session failed closed")]
    SessionFailed,
    /// An exit-reported failure reached the local route client.
    #[error("{0}")]
    ExitFailure(OnionExitFailure),
}

/// Why an `https` exit returned no response: the encoded failure of a fetch outcome. It carries
/// no local diagnostic, so a failure reveals nothing of the exit beyond its class.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnionExitFailure {
    /// The exit policy or its byte budget denied the fetch.
    PermissionDenied,
    /// The session's stream did not decode as a request.
    MalformedRequest,
    /// The fetch failed at the exit.
    Internal,
}

impl OnionExitFailure {
    /// Convert a local node error into a wire failure at the adapter boundary.
    pub fn from_error(error: &Error) -> Self {
        match error {
            Error::NoPermission => Self::PermissionDenied,
            _ => Self::Internal,
        }
    }
}

impl fmt::Display for OnionExitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PermissionDenied => Error::NoPermission.fmt(f),
            Self::MalformedRequest => f.write_str("onion exit received a malformed HTTPS request"),
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
