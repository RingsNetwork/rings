#![warn(missing_docs)]
//! Transport relay — one abstraction for TCP, HTTP and (future) UDP.
//!
//! # Why these are *not* three protocols
//!
//! The rings overlay (DHT + swarm + backend envelopes) already provides a **reliable,
//! ordered, bidirectional message channel between two DIDs** — call it the *virtual
//! circuit*. TCP / HTTP / UDP "services" are all the **same thing**: a *relay* that
//! maps a local I/O resource (a socket) onto that virtual circuit. They differ only in
//! the shape of the local resource, along three axes:
//!
//! ```text
//!   axis                     TCP                 HTTP                    UDP
//!   ----------------------   -----------------   ---------------------   ------------------
//!   session cardinality      ω (endless stream)  1  (one req/resp,       0  (no session,
//!                                                    affine: Req ⊸ Resp)     datagrams)
//!   framing                  byte stream         HTTP messages           datagrams
//!   lifecycle                open → data* → close open → 1×req → 1×resp   none
//!                                                  → close
//!   ordering / reliability   ordered, reliable   ordered, reliable       unordered, lossy
//!                                                                         (semantics chosen
//!                                                                          when tunnelled)
//! ```
//!
//! Categorically they are one structure at three points of a single "session
//! cardinality" axis:
//!
//! - **TCP** = a bidirectional byte **stream** — the cofree stream / a long-lived
//!   process; cardinality **ω**.
//! - **HTTP** = the **affine** degeneration of TCP: exactly one exchange
//!   `Request ⊸ Response` (a use-once session); cardinality **1**.
//! - **UDP** = the **0-session** degeneration: `Datagram → [Datagram]`, a discrete
//!   transducer with no lifecycle; cardinality **0**.
//!
//! So adding UDP later is not a fourth subsystem — it is this axis taken to 0.
//!
//! # How it sits on the effect base (`backend::ext`)
//!
//! Pure/effect separation is preserved:
//!
//! - The **interpreter owns the live resources** (the `TcpStream` / `UdpSocket`), keyed
//!   by [`SessionId`], in a resource table. These are non-purifiable OS handles and so
//!   live only in the imperative shell — never in a protocol's state.
//! - A protocol's **pure `step`** holds only session *metadata* (which `SessionId` maps
//!   to which peer/service, framing state, counters) — never a live socket.
//! - Generic transport **effects** (run by the interpreter): stream ops
//!   `Connect` / `Write` / `Close`; datagram ops `Bind` / `SendTo`.
//! - Local reads / accepts **re-inject** [`Frame`]s as events (the event trace of the
//!   effect monad): the read task feeds `Data` / `Close` / `Datagram` back through the
//!   router → `step` → an `Effect::Send` over the virtual circuit.
//!
//! TCP / HTTP / UDP are then thin instances over this one relay: TCP uses the stream
//! ops with an ω session; HTTP adds "one request → one response → close" session logic
//! (expressible purely in `step`); UDP uses only the datagram ops with no session.

// The relay's imperative resource tables are private to the relay interpreter — not a public
// API. Reachable in-crate by the relay extension only.
#[cfg(feature = "node")]
pub(crate) mod engine;
#[cfg(feature = "browser")]
pub(crate) mod wt;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

/// Identifier of a relayed session/flow (a virtual circuit ↔ local socket pairing).
///
/// TCP uses it for a connection; UDP uses it for a *flow* (a NAT-like mapping that
/// routes responses back to the right local client) — see [`TransportKind`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// Which end **opened** a relay session, from the perspective of the node holding the key.
///
/// Necessary because two nodes that simultaneously open a tunnel to each other both mint
/// `SessionId(0)`: without an initiator, "the session I opened to peer B" and "the session B
/// opened to me" would collide on `(peer=B, namespace, session=0)`, and a wire `Data(0)`
/// would be ambiguous. The initiator splits the id space into two halves per `(peer,
/// namespace)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Initiator {
    /// This node opened the session (a client tunnel).
    Local,
    /// The peer opened the session (this node is the server).
    Remote,
}

impl Initiator {
    /// The other end's view of the same session.
    pub fn flip(self) -> Self {
        match self {
            Initiator::Local => Initiator::Remote,
            Initiator::Remote => Initiator::Local,
        }
    }
}

/// A relay session's full identity — the unit used to key live sessions and to address
/// transport effects.
///
/// A bare [`SessionId`] is **not** a valid address: the id on the wire is assigned by the
/// opener, so two ends can both pick `SessionId(0)`. The key scopes a session by `(peer,
/// namespace, session, initiator)`, where `peer` is the **authenticated** other end
/// (`event.from`, the verified signer) and `initiator` records which end opened it. Because a
/// peer cannot forge `event.from`, it can only ever address sessions whose `peer` is itself
/// (owner rejection); and `initiator` keeps a peer's session distinct from one of ours that
/// happened to get the same id (bidirectional-open safety).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SessionKey {
    /// The authenticated remote end of the session (`event.from` for inbound frames).
    pub peer: Did,
    /// The transport namespace the session lives under (e.g. `tcp`, `udp`).
    pub namespace: String,
    /// The opener-assigned session id, unique only within `(peer, namespace, initiator)`.
    pub session: SessionId,
    /// Which end opened the session (disambiguates colliding ids on simultaneous open).
    pub initiator: Initiator,
}

impl SessionKey {
    /// Build a session key from its parts.
    pub fn new(
        peer: Did,
        namespace: impl Into<String>,
        session: SessionId,
        initiator: Initiator,
    ) -> Self {
        Self {
            peer,
            namespace: namespace.into(),
            session,
            initiator,
        }
    }
}

/// Which local socket a relay session is backed by.
///
/// Both kinds share the same [`Frame`] vocabulary (`Open`/`Data`/`Close`); only the
/// socket differs. UDP is *flow*-based rather than truly sessionless because a relayed
/// datagram still needs a return path to the originating local client, so each flow
/// carries a [`SessionId`] just like a TCP connection. `Data` preserves message
/// boundaries (one datagram per frame) for UDP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportKind {
    /// Connection-oriented byte stream.
    Tcp,
    /// Datagram flow (per-flow socket; message boundaries preserved per `Data`).
    Udp,
}

/// The relay's overlay wire message — the payload carried under a transport namespace.
///
/// One vocabulary for both kinds (TCP connections and UDP flows):
///
/// ```text
///   Open(session, service) → Data(session, bytes)* → Close(session)
/// ```
///
/// `Open` is always sent by the session's opener. `Data`/`Shutdown`/`Close` flow in both
/// directions over the *same* opener-assigned id, so they carry `from_opener` — whether the
/// **sender** of this frame opened the session. The receiver flips it to recover its own
/// [`Initiator`], so a peer's session never collides with one of ours sharing the same id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Frame {
    /// Open a session/flow to a named local service (always sent by the opener).
    Open {
        /// Session identifier (assigned by the opener).
        session: SessionId,
        /// Local service name to connect to.
        service: String,
    },
    /// Bytes on an open session (one datagram per frame for UDP).
    Data {
        /// Session the bytes belong to.
        session: SessionId,
        /// Whether the sender of this frame opened the session.
        from_opener: bool,
        /// Payload bytes.
        bytes: Bytes,
    },
    /// Half-close: the sender has no more `Data` this direction (a TCP FIN). The
    /// receiver shuts down its local write side but keeps the reverse direction open.
    /// Ignored by UDP (datagram flows have no half-close).
    Shutdown {
        /// Session being half-closed.
        session: SessionId,
        /// Whether the sender of this frame opened the session.
        from_opener: bool,
    },
    /// Close a session/flow (full teardown, both directions).
    Close {
        /// Session to close.
        session: SessionId,
        /// Whether the sender of this frame opened the session.
        from_opener: bool,
    },
}
