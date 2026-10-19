//! The decode boundary of the data plane: a peer's cell, or the node's own reconcile.
//!
//! ```text
//! decode(from, w) = Cell(from, parse(w))           from ≠ me    w is exactly one class length
//!                 = Link(Reconcile)                from = me    injected once, at install
//! link_fact(f)    = Link(Opened | Closed | Dropped (f.peer))    core's facts, in queue order
//! ```
//!
//! A peer's payload is the raw cell `α‖β‖γ‖y`, with no framing (#834 D6, `F = 0`): the only
//! check here is the width, the class being the length. A payload of any other length is
//! rejected before it is copied.

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::extension::ext::LinkFact;
use crate::extension::ext::Reject;
use crate::extension::ext::Wire;
use crate::onion::sphinx::cell::OnionCell;

/// A fact about this node's links, in the order the data plane's queue delivers it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum OnionLinkFact {
    /// Core admitted the peer's link (`LinkFact::Admitted`).
    Opened(Did),
    /// Core retired the peer's link (`LinkFact::Retired`, #963).
    Closed(Did),
    /// A message from the peer, whose link stays up, was dropped here for want of room
    /// (`LinkFact::Dropped`, #966, #969): it may have been a cell of a session at this node.
    Dropped(Did),
    /// Reconcile the link table with core's admitted peers, read when the shell applies it:
    /// injected once when the data plane is installed, so the links core admitted before its
    /// namespace was registered are live (the linearisation obligation of `admission`).
    Reconcile,
}

impl From<LinkFact> for OnionLinkFact {
    /// Core's link fact as the data plane's.
    fn from(fact: LinkFact) -> Self {
        match fact {
            LinkFact::Admitted(peer) => Self::Opened(peer),
            LinkFact::Retired(peer) => Self::Closed(peer),
            LinkFact::Dropped(peer) => Self::Dropped(peer),
        }
    }
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
