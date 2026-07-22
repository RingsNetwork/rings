use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::WebrtcConnectionState;

use super::SwarmConnection;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;

/// Maximum number of peers that may be handshaking before a data channel opens.
pub(crate) const DEFAULT_PENDING_CONNECTION_CAPACITY: usize = 32;

pub(super) const PENDING_CONNECTION_TIMEOUT_MS: i64 = 180_000;

/// Identifies one pending handshake for a peer.
///
/// A peer can have a replacement handshake after a timeout. Callbacks carry
/// this token so a late callback from the replaced connection cannot promote
/// the newer handshake into the active routing set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingConnectionAttempt {
    pub(super) peer: Did,
    pub(super) generation: u64,
}

impl PendingConnectionAttempt {
    pub(crate) fn peer(self) -> Did {
        self.peer
    }
}

#[derive(Debug)]
pub(super) struct PendingPeer {
    generation: u64,
    admitted_at_ms: i64,
}

/// Bounded, non-routable handshakes owned by the swarm lifecycle.
///
/// The pool deliberately has no DHT reference: a peer is visible to Chord
/// only after its data channel opens and the matching attempt is promoted.
pub(super) struct PendingPeerPool<const MAX_PENDING: usize> {
    next_generation: u64,
    peers: BTreeMap<Did, PendingPeer>,
}

impl<const MAX_PENDING: usize> PendingPeerPool<MAX_PENDING> {
    pub(super) fn new() -> Self {
        Self {
            next_generation: 0,
            peers: BTreeMap::new(),
        }
    }

    pub(super) fn reserve(&mut self, peer: Did, now_ms: i64) -> Result<PendingConnectionAttempt> {
        if self.peers.contains_key(&peer) {
            return Err(Error::AlreadyConnected);
        }
        if self.peers.len() >= MAX_PENDING {
            return Err(Error::PendingConnectionCapacityExceeded {
                capacity: MAX_PENDING,
            });
        }

        self.next_generation = self.next_generation.wrapping_add(1);
        let attempt = PendingConnectionAttempt {
            peer,
            generation: self.next_generation,
        };
        self.peers.insert(peer, PendingPeer {
            generation: attempt.generation,
            admitted_at_ms: now_ms,
        });
        Ok(attempt)
    }

    pub(super) fn contains(&self, peer: Did) -> bool {
        self.peers.contains_key(&peer)
    }

    pub(super) fn remove(&mut self, attempt: PendingConnectionAttempt) -> bool {
        let Some(peer) = self.peers.get(&attempt.peer) else {
            return false;
        };
        if peer.generation != attempt.generation {
            return false;
        }
        self.peers.remove(&attempt.peer);
        true
    }

    pub(super) fn expire(&mut self, now_ms: i64) -> Vec<PendingConnectionAttempt> {
        let expired = self
            .peers
            .iter()
            .filter_map(|(peer, pending)| {
                (now_ms.saturating_sub(pending.admitted_at_ms) >= PENDING_CONNECTION_TIMEOUT_MS)
                    .then_some(PendingConnectionAttempt {
                        peer: *peer,
                        generation: pending.generation,
                    })
            })
            .collect::<Vec<_>>();
        for attempt in &expired {
            self.peers.remove(&attempt.peer);
        }
        expired
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(super) fn len(&self) -> usize {
        self.peers.len()
    }
}

impl SwarmTransport {
    fn pending_peers(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PendingPeerPool<DEFAULT_PENDING_CONNECTION_CAPACITY>>>
    {
        self.pending_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    pub(super) fn active_peers(&self) -> Result<std::sync::MutexGuard<'_, BTreeSet<Did>>> {
        self.active_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    pub(super) fn get_raw_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    pub(super) fn is_pending_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.pending_peers()?.contains(peer))
    }

    /// Return whether `peer` completed a handshake and still owns a logical slot.
    ///
    /// Unlike [`Self::is_active_connection`], this remains true while a terminal
    /// callback removes the peer, so lifecycle cleanup can evict it from the DHT
    /// even after WebRTC reports `Closed`.
    pub(crate) fn is_admitted_connection(&self, peer: Did) -> bool {
        self.active_peers()
            .map(|active| active.contains(&peer))
            .unwrap_or(false)
    }

    /// Return whether `peer` completed its pending handshake and can route traffic.
    ///
    /// Data-channel open is the admission boundary. WebRTC may still report a
    /// transient `Disconnected` state after admission, so only terminal states
    /// make an admitted peer non-routable before its callback removes the slot.
    pub(crate) fn is_active_connection(&self, peer: Did) -> bool {
        self.is_admitted_connection(peer)
            && self.get_raw_connection(peer).is_some_and(|connection| {
                !matches!(
                    connection.webrtc_connection_state(),
                    WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
                )
            })
    }

    pub(super) async fn reserve_pending_connection(
        &self,
        peer: Did,
    ) -> Result<PendingConnectionAttempt> {
        self.expire_pending_connections().await?;
        if peer == self.dht.did {
            return Err(Error::ShouldNotConnectSelf);
        }
        // A peer keeps its active slot through transient WebRTC state changes
        // until its terminal callback removes it from the DHT. Do not admit a
        // second pending handshake for that DID during this interval.
        if self.is_admitted_connection(peer) {
            return Err(Error::AlreadyConnected);
        }
        self.pending_peers()?.reserve(peer, get_epoch_ms_i64())
    }

    pub(super) fn pending_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        let pending = self.pending_peers()?;
        Ok(pending
            .peers
            .get(&peer)
            .map(|pending| PendingConnectionAttempt {
                peer,
                generation: pending.generation,
            }))
    }

    pub(crate) fn promote_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let mut pending = self.pending_peers()?;
        if !pending.remove(attempt) {
            return Ok(false);
        }
        drop(pending);
        self.active_peers()?.insert(attempt.peer);
        Ok(true)
    }

    fn retire_pending_connection(&self, attempt: PendingConnectionAttempt) -> Result<bool> {
        Ok(self.pending_peers()?.remove(attempt))
    }

    pub(super) fn retire_active_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.active_peers()?.remove(&peer))
    }

    /// Cancel a current pending handshake and release its non-routable transport object.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        if !self.retire_pending_connection(attempt)? {
            return Ok(false);
        }
        self.transport
            .close_connection(&attempt.peer.to_string())
            .await
            .map_err(Error::Transport)?;
        Ok(true)
    }

    pub(super) async fn abandon_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
    ) {
        if let Err(error) = self.cancel_pending_connection(attempt).await {
            tracing::warn!(
                "failed to cancel pending connection to {} after {operation}: {error}",
                attempt.peer
            );
        }
    }

    /// Close pending handshakes whose data channel did not open before the deadline.
    ///
    /// These peers have never entered the DHT, so expiry only releases the
    /// transport object; it deliberately performs no topology mutation.
    pub(crate) async fn expire_pending_connections(&self) -> Result<()> {
        let expired = self.pending_peers()?.expire(get_epoch_ms_i64());
        for attempt in expired {
            tracing::warn!("pending connection to {} timed out", attempt.peer);
            self.transport
                .close_connection(&attempt.peer.to_string())
                .await
                .map_err(Error::Transport)?;
        }
        Ok(())
    }

    /// Create a new non-routable transport connection and register its pending attempt.
    pub(super) async fn new_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: InnerSwarmCallback,
    ) -> Result<()> {
        let cid = attempt.peer.to_string();
        if let Err(error) = self
            .transport
            .new_connection(&cid, Box::new(callback))
            .await
        {
            let _ = self.retire_pending_connection(attempt);
            return Err(Error::Transport(error));
        }
        Ok(())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn pending_connection_count(&self) -> Result<usize> {
        Ok(self.pending_peers()?.len())
    }
}
