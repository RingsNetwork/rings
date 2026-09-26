//! The decode boundary of the data plane: a peer's cell, or one of the node's own link facts.
//!
//! ```text
//! decode(from, w) = Cell(from, parse(w))           from ≠ me    w is exactly one class length
//!                 = Link(fact)                     from = me    a fact the link feed injected
//! ```
//!
//! A peer's payload is the raw cell `α‖β‖γ‖y`, with no framing (#834 D6, `F = 0`): the only
//! check here is the width, the class being the length. A payload of any other length is
//! rejected before it is copied.

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::swarm::callback::PeerLink;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::Reject;
use crate::extension::ext::Wire;
use crate::onion::sphinx::cell::OnionCell;

/// A fact about this node's links, injected by the link feed in the order core reported it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum OnionLinkFact {
    /// Core admitted the link.
    Opened(PeerLink),
    /// Core retired the link.
    Closed(PeerLink),
    /// Reconcile the link table with core's registry snapshot, read when the fact was queued:
    /// its place in the feed's FIFO is the instant it was read, so every fact before it is older
    /// and every fact after it agrees with it or is newer (the linearisation obligation).
    Reconcile(Vec<PeerLink>),
}

impl OnionLinkFact {
    /// The fact's encoding as a self-injected payload.
    pub(crate) fn encode(&self) -> Result<Bytes> {
        rings_codec::serialize(self)
            .map(Bytes::from)
            .map_err(|_| Error::EncodeError)
    }
}

/// The typed input of the data plane's reducer.
#[derive(Debug)]
pub(crate) enum OnionCircuitInput {
    /// A cell received from the peer `from` over their direct link.
    Cell {
        /// The authenticated previous hop.
        from: Did,
        /// The cell, parsed by its length.
        cell: OnionCell,
    },
    /// A link fact.
    Link(OnionLinkFact),
}

/// One typed onion-circuit input accepted by the reducer.
#[derive(Debug)]
pub(crate) struct OnionCircuitEvent {
    pub(super) input: OnionCircuitInput,
}

/// `decode` of the module documentation.
pub(super) fn decode_event(wire: Wire<'_>) -> std::result::Result<OnionCircuitEvent, Reject> {
    let input = if wire.from == wire.me {
        rings_codec::deserialize::<OnionLinkFact>(wire.payload)
            .map(OnionCircuitInput::Link)
            .map_err(|error| Reject(format!("bad onion link fact: {error}")))?
    } else {
        OnionCircuitInput::Cell {
            from: wire.from,
            cell: OnionCell::parse(wire.payload).map_err(|error| Reject(error.to_string()))?,
        }
    };
    Ok(OnionCircuitEvent { input })
}
