#![deny(missing_docs)]

//! The boundary between core and the application (#935).
//!
//! Core defines, handles and judges every protocol message itself. The application sees only
//! what core hands it through this boundary:
//!
//! ```text
//!   core inbound lanes ──deliver (queued, or dropped)──┐
//!   relay-inbox drain  ──try_reserve ▸ fill───────────┤
//!   connection lifecycle ──announce───────────────────┼──▶ ApplicationInbox::next ──▶ app
//!   successor lookups ──answer──▶ SuccessorReport ─────┘ (a handle the app awaited itself)
//!   delegated namespaces ◀── registered in advance by the app, read by core as data
//! ```
//!
//! Laws:
//! - **L0.** Core never calls, awaits or waits for application code. A full inbox drops further
//!   live application data, counted, as a best-effort network drops it; no lane and no
//!   lifecycle transition ever waits for the application to read (#913 R10). Stored relay mail
//!   is offered only when it fits, and is retired only once the application takes it.
//! - Only [`ApplicationData`] crosses the boundary: no control message has a representation
//!   here, and the projection `ApplicationData::project` is total over [`Message`] variants, so
//!   a new variant fails to compile until it is classified.
//! - Events are the logical facts [`SwarmEvent::PeerAdmitted`] and [`SwarmEvent::PeerRetired`],
//!   never a physical connection state.

mod delegation;
mod inbox;
mod lookup;
mod relay;

use std::sync::Arc;

use delegation::DelegatedNamespaces;
pub use inbox::ApplicationInbox;
use inbox::ApplicationOutlet;
pub(crate) use inbox::Delivered;
pub use inbox::APPLICATION_INBOX_WEIGHT;
use lookup::ApplicationLookups;
pub use lookup::SuccessorReport;
pub(crate) use relay::RelayConsumption;
use relay::RelayInFlight;

use crate::dht::Did;
use crate::message::e2e::E2eHandshakeRequest;
use crate::message::e2e::E2eHandshakeResponse;
use crate::message::e2e::E2eStreamFrame;
use crate::message::CustomMessage;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;

/// Core's side of the boundary, held by the transport: everything core hands the application,
/// and the one piece of application configuration core reads.
pub(crate) struct ApplicationPort {
    /// The inbox core writes.
    pub(crate) outlet: ApplicationOutlet,
    /// The application's outstanding successor lookups.
    pub(crate) lookups: Arc<ApplicationLookups>,
    /// The namespaces whose admission the application delegated.
    pub(crate) delegations: DelegatedNamespaces,
    /// Relay mail handed to the inbox and not yet retired from storage.
    pub(crate) relay_in_flight: RelayInFlight,
}

impl ApplicationPort {
    /// A port with an unclaimed inbox, no lookups and no delegation.
    pub(crate) fn new() -> Self {
        Self {
            outlet: ApplicationOutlet::new(),
            lookups: Arc::new(ApplicationLookups::default()),
            delegations: DelegatedNamespaces::default(),
            relay_in_flight: RelayInFlight::default(),
        }
    }
}

/// The body of a message core hands to the application: the message classes that carry
/// application data. Every other class is core's own.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationData {
    /// An application message (`CustomMessage`).
    Custom(CustomMessage),
    /// A request to negotiate E2E encryption.
    E2eHandshakeRequest(E2eHandshakeRequest),
    /// An acceptance of E2E encryption.
    E2eHandshakeResponse(E2eHandshakeResponse),
    /// One encrypted E2E stream frame.
    E2eStreamFrame(E2eStreamFrame),
}

impl ApplicationData {
    /// The application data `message` carries, if it carries any.
    ///
    /// Law: total and exhaustive over [`Message`], so `project(m) = Some(_)` exactly for the
    /// `Application` and `E2e` classes other than a chunk, which is reassembled by core first.
    pub(crate) fn project(message: Message) -> Option<Self> {
        match message {
            Message::CustomMessage(message) => Some(Self::Custom(message)),
            Message::E2eHandshakeRequest(request) => Some(Self::E2eHandshakeRequest(request)),
            Message::E2eHandshakeResponse(response) => Some(Self::E2eHandshakeResponse(response)),
            Message::E2eStreamFrame(frame) => Some(Self::E2eStreamFrame(frame)),
            Message::ConnectNodeSend(_)
            | Message::ConnectNodeReport(_)
            | Message::FindSuccessorSend(_)
            | Message::FindSuccessorReport(_)
            | Message::NotifyPredecessorSend(_)
            | Message::ProbeRequest(_)
            | Message::ProbeOffer(_)
            | Message::SearchEntry(_)
            | Message::FoundEntry(_)
            | Message::OperateEntry(_)
            | Message::SyncEntriesWithSuccessor(_)
            | Message::SyncEntriesWithSuccessorReport(_)
            | Message::QueryForTopoInfoSend(_)
            | Message::QueryForTopoInfoReport(_)
            | Message::Chunk(_)
            | Message::ProbeAcknowledgement(_) => None,
        }
    }

    /// The heap bytes this body owns beyond its own size, part of the measure of
    /// [`APPLICATION_INBOX_WEIGHT`].
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Custom(CustomMessage(bytes)) => bytes.capacity(),
            Self::E2eHandshakeRequest(_) | Self::E2eHandshakeResponse(_) => 0,
            Self::E2eStreamFrame(frame) => std::mem::size_of_val(frame.ciphertext.as_slice()),
        }
    }
}

/// One application message addressed to this node, as core admitted and handled it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationMessage {
    /// The transaction the message arrived under.
    pub tx_id: uuid::Uuid,
    /// The authenticated signer of the message.
    pub from: Did,
    /// The application data.
    pub data: ApplicationData,
}

impl ApplicationMessage {
    /// The application message `payload` carries for this node, if its body is application
    /// data. `message` is the body already decoded from `payload`.
    pub(crate) fn project(payload: &MessagePayload, message: Message) -> Option<Self> {
        ApplicationData::project(message).map(|data| Self {
            tx_id: payload.transaction.tx_id,
            from: payload.transaction.signer(),
            data,
        })
    }

    /// The bytes this message retains while it waits in the inbox: its own size and the heap
    /// bytes of its data, so an empty message still counts (#913 R10 M1).
    fn weight(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.data.heap_bytes())
    }
}

/// A logical fact about a peer, as core concluded it.
///
/// Law: per peer, the inbox yields `PeerAdmitted` and `PeerRetired` alternately, starting with
/// `PeerAdmitted`, in the order core decided them. An admission retired before the application
/// read it is withdrawn together with its retirement, so an application never reads a
/// retirement of an admission it did not read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SwarmEvent {
    /// `peer`'s data channel is open and it joined the local DHT.
    PeerAdmitted {
        /// The admitted peer.
        peer: Did,
    },
    /// `peer`'s admitted connection record was retired: it left the local DHT, whatever
    /// physical event or local decision caused it.
    PeerRetired {
        /// The retired peer.
        peer: Did,
    },
}

impl SwarmEvent {
    /// The peer this fact is about.
    pub fn peer(&self) -> Did {
        match *self {
            Self::PeerAdmitted { peer } | Self::PeerRetired { peer } => peer,
        }
    }
}

/// One item of the application inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplicationDelivery {
    /// Application data addressed to this node.
    Message(ApplicationMessage),
    /// A logical fact about a peer.
    Event(SwarmEvent),
}
