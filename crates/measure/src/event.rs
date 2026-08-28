use std::num::NonZeroU64;

use serde::Deserialize;
use serde::Serialize;

/// Whether the runtime authenticated the stable identity associated with an event.
///
/// This is an explicit proof token at the pure transition boundary. A runtime
/// whose transport only emits post-verification events will pass
/// [`Authentication::Authenticated`]; pre-authentication ingress can use the
/// other variant without gaining credit. Keeping the axis in the pure model
/// makes the identity gate testable without coupling this crate to a protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authentication {
    /// The event belongs to a cryptographically authenticated stable peer identity.
    Authenticated,
    /// The transport has not authenticated the stable peer identity.
    Unauthenticated,
}

/// One logical local observation about a remote peer.
///
/// Successful transfer variants carry useful payload bytes. Framing, duplicate
/// chunks, retransmissions, and unverified bytes are not useful payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeasurementEvent {
    /// A connection attempt completed successfully.
    Connected,
    /// An established or attempted connection ended unsuccessfully.
    Disconnected,
    /// One logical message was delivered to the peer.
    Sent {
        /// Useful payload bytes delivered to the peer.
        useful_bytes: u64,
    },
    /// One logical message could not be delivered to the peer.
    FailedToSend,
    /// One logical message from the peer was fully received and verified.
    Received {
        /// Useful payload bytes received and verified from the peer.
        useful_bytes: u64,
    },
    /// One logical inbound message failed reassembly, decoding, or verification.
    FailedToReceive,
}

/// One or more homogeneous logical observations applied atomically.
///
/// For successful transfer events, `useful_bytes` is the aggregate useful
/// payload across every occurrence. Reliability evidence advances by
/// [`Self::occurrences`], while byte credit advances once by that aggregate.
/// This representation lets effect adapters coalesce events in constant space
/// without inventing zero-byte messages or exposing partial transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasurementBatch {
    event: MeasurementEvent,
    occurrences: NonZeroU64,
}

impl MeasurementBatch {
    /// Construct an atomic homogeneous batch.
    pub const fn new(event: MeasurementEvent, occurrences: NonZeroU64) -> Self {
        Self { event, occurrences }
    }

    /// Construct a batch containing exactly one observation.
    pub const fn single(event: MeasurementEvent) -> Self {
        Self::new(event, NonZeroU64::MIN)
    }

    /// Aggregate event, including aggregate useful bytes for transfers.
    pub const fn event(self) -> MeasurementEvent {
        self.event
    }

    /// Number of logical observations represented by this batch.
    pub const fn occurrences(self) -> NonZeroU64 {
        self.occurrences
    }
}

/// A named counter whose checked update can overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Useful payload bytes sent to a peer.
    BytesSent,
    /// Useful payload bytes received from a peer.
    BytesReceived,
    /// Successful connection observations.
    Connected,
    /// Disconnection observations.
    Disconnected,
    /// Successful logical sends.
    Sent,
    /// Failed logical sends.
    FailedToSend,
    /// Successful verified logical receives.
    Received,
    /// Failed logical receives.
    FailedToReceive,
}

/// Result of applying an observation at the authentication boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The authenticated observation changed the peer record.
    Applied,
    /// The observation was ignored because no stable peer identity was authenticated.
    IgnoredUnauthenticated,
}
