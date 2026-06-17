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

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

/// Identifier of a relayed session (a virtual circuit ↔ local socket pairing).
///
/// Connection-oriented transports (TCP, HTTP) use it to address a specific
/// connection; connectionless transports (UDP) ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// The relay's overlay wire message — the payload carried under a transport namespace.
///
/// Unifies the stream and datagram shapes; a given transport uses the subset matching
/// its session cardinality:
///
/// ```text
///   TCP  (ω): Open, Data, Close
///   HTTP (1): Open, Data, Close   (with one-shot session logic in `step`)
///   UDP  (0): Datagram
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Frame {
    /// Open a connection-oriented session to a named local service.
    Open {
        /// Session identifier (assigned by the dialing side).
        session: SessionId,
        /// Local service name to connect to.
        service: String,
    },
    /// Bytes on an open session (stream framing).
    Data {
        /// Session the bytes belong to.
        session: SessionId,
        /// Payload bytes.
        bytes: Bytes,
    },
    /// Close a session.
    Close {
        /// Session to close.
        session: SessionId,
    },
    /// A connectionless datagram (no session).
    Datagram {
        /// Datagram bytes.
        bytes: Bytes,
    },
}
