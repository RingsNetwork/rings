//! The admitted edges of a node: protocol state a node-layer consumer reads and awaits (#855).
//!
//! An edge is admitted from its [`SwarmEvent::PeerAdmitted`] until its
//! [`SwarmEvent::PeerRetired`]: its transport is ready and it joined the local DHT.
//! [`Swarm::admitted_edges`] reads how many there are now, and [`Swarm::edges_changed`] resolves
//! at the next admission or retirement after its call (the edge-watch law of
//! `fair_admission::slot_roster`). A consumer awaiting a predicate `p` of the count takes the
//! change before it reads the count, so no change between the two is missed:
//!
//! ```text
//!   loop { changed ← edges_changed(); if p(admitted_edges()) { done } await changed }
//! ```
//!
//! [`SwarmEvent::PeerAdmitted`]: crate::swarm::application::SwarmEvent::PeerAdmitted
//! [`SwarmEvent::PeerRetired`]: crate::swarm::application::SwarmEvent::PeerRetired
//! [`Swarm::admitted_edges`]: crate::swarm::Swarm::admitted_edges
//! [`Swarm::edges_changed`]: crate::swarm::Swarm::edges_changed

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use event_listener::EventListener;

/// Resolves at the next admission or retirement of an edge after it was taken.
pub struct EdgesChanged(pub(crate) EventListener);

impl Future for EdgesChanged {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl std::fmt::Debug for EdgesChanged {
    /// The listener is opaque: whether a change came is learned only by awaiting it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EdgesChanged")
    }
}
