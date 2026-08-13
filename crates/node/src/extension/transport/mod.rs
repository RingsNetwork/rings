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
#[cfg(rings_native)]
pub(crate) mod engine;
pub(crate) mod platform;
#[cfg(rings_browser)]
pub(crate) mod wt;

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
#[cfg(rings_native)]
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use serde::Deserialize;
use serde::Serialize;

/// Maximum silence before an abandoned local socket/flow is reclaimed.
#[cfg(rings_native)]
pub(crate) const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Allocate one counter value without ever wrapping back to a live ABA-equivalent value.
pub(crate) fn allocate_non_reusing(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()
}

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

/// Result of applying one ordered peer-to-local transport effect without
/// waiting for backend backpressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EffectEnqueue {
    /// The effect was accepted by the current backend generation.
    Enqueued,
    /// No backend generation exists; pure state must simply forget the key.
    Missing,
    /// The current backend generation failed or saturated; pure state must
    /// forget it and notify the peer with `Close`.
    Failed,
}

/// Maximum number of ordered peer-to-local operations retained by a browser transport session.
#[cfg(any(test, rings_browser))]
pub(crate) const MAX_OUTBOUND_QUEUE_OPS: usize = 1024;

/// Maximum aggregate payload retained by a browser transport session.
#[cfg(any(test, rings_browser))]
pub(crate) const MAX_OUTBOUND_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// Pure resource account for a browser transport's deferred operation trace.
///
/// Invariant: `operations <= MAX_OUTBOUND_QUEUE_OPS` and
/// `data_bytes <= MAX_OUTBOUND_QUEUE_BYTES` after every successful reservation.
#[derive(Default)]
#[cfg(any(test, rings_browser))]
pub(crate) struct OutboundQueueBudget {
    operations: usize,
    data_bytes: usize,
}

/// Pure ownership state for the single browser writer-drain task.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg(any(test, rings_browser))]
pub(crate) enum OutboundDrainState {
    /// No task owns the queue; the next successful enqueue must start one.
    #[default]
    Idle,
    /// Exactly one task owns the queue and will observe later enqueues.
    Active,
}

#[cfg(any(test, rings_browser))]
impl OutboundDrainState {
    /// Claim an idle queue. Returns whether the caller acquired drain ownership.
    pub(crate) fn claim(&mut self) -> bool {
        match self {
            Self::Idle => {
                *self = Self::Active;
                true
            }
            Self::Active => false,
        }
    }

    /// Release drain ownership after observing an empty queue.
    pub(crate) fn release(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(any(test, rings_browser))]
impl OutboundQueueBudget {
    /// Reserve one operation carrying `data_bytes`, or leave the budget unchanged.
    pub(crate) fn try_reserve(&mut self, data_bytes: usize) -> bool {
        let Some(operations) = self.operations.checked_add(1) else {
            return false;
        };
        let Some(total_bytes) = self.data_bytes.checked_add(data_bytes) else {
            return false;
        };
        if operations > MAX_OUTBOUND_QUEUE_OPS || total_bytes > MAX_OUTBOUND_QUEUE_BYTES {
            return false;
        }
        self.operations = operations;
        self.data_bytes = total_bytes;
        true
    }

    /// Release one previously reserved operation, or leave the budget unchanged when the
    /// supplied values do not refine the current state.
    pub(crate) fn release(&mut self, data_bytes: usize) -> bool {
        let Some(operations) = self.operations.checked_sub(1) else {
            return false;
        };
        let Some(total_bytes) = self.data_bytes.checked_sub(data_bytes) else {
            return false;
        };
        self.operations = operations;
        self.data_bytes = total_bytes;
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_budget_preserves_both_operation_and_byte_bounds() {
        let mut operation_bound = OutboundQueueBudget::default();
        for _ in 0..MAX_OUTBOUND_QUEUE_OPS {
            assert!(operation_bound.try_reserve(0));
        }
        assert!(!operation_bound.try_reserve(0));

        let mut byte_bound = OutboundQueueBudget::default();
        assert!(byte_bound.try_reserve(MAX_OUTBOUND_QUEUE_BYTES));
        assert!(!byte_bound.try_reserve(1));
    }

    #[test]
    fn rejected_outbound_budget_reservation_does_not_consume_capacity() {
        let mut budget = OutboundQueueBudget::default();
        assert!(!budget.try_reserve(MAX_OUTBOUND_QUEUE_BYTES + 1));
        assert!(budget.try_reserve(MAX_OUTBOUND_QUEUE_BYTES));
    }

    #[test]
    fn released_outbound_budget_can_be_reserved_again() {
        let mut budget = OutboundQueueBudget::default();
        assert!(budget.try_reserve(MAX_OUTBOUND_QUEUE_BYTES));
        assert!(budget.release(MAX_OUTBOUND_QUEUE_BYTES));
        assert!(budget.try_reserve(MAX_OUTBOUND_QUEUE_BYTES));
    }

    #[test]
    fn invalid_outbound_budget_release_is_total_and_does_not_mutate() {
        let mut budget = OutboundQueueBudget::default();
        assert!(budget.try_reserve(4));
        assert!(!budget.release(5));
        assert!(budget.release(4));
        assert!(!budget.release(0));
    }

    #[test]
    fn outbound_drain_has_exactly_one_owner_until_empty() {
        let mut drain = OutboundDrainState::Idle;
        assert!(drain.claim());
        assert!(!drain.claim());
        drain.release();
        assert!(drain.claim());
    }

    #[test]
    fn non_reusing_allocator_is_total_at_exhaustion() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_non_reusing(&counter), Some(u64::MAX - 1));
        assert_eq!(allocate_non_reusing(&counter), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
}
