use std::time::Duration;

use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;

use super::SwarmConnection;
use super::SwarmTransport;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::utils::sleep;

const DATA_CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(8);

impl SwarmTransport {
    /// Get an active, routable connection by DID.
    ///
    /// Pending and terminal physical transports are intentionally invisible here.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.is_active_connection(peer)
            .then(|| self.get_raw_connection(peer))
            .flatten()
    }

    /// Get all active, routable transport connections.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.active_peer_ids()
            .into_iter()
            .filter_map(|peer| {
                self.get_connection(peer)
                    .map(|connection| (peer, connection))
            })
            .collect()
    }

    fn active_peer_ids(&self) -> Vec<Did> {
        self.active_peers()
            .map(|active| active.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Return admitted transports, including a terminal connection that still
    /// needs lifecycle cleanup. This is deliberately internal: callers outside
    /// the swarm only observe routable connections through [`Self::get_connections`].
    pub(crate) fn admitted_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.active_peer_ids()
            .into_iter()
            .filter_map(|peer| {
                self.get_raw_connection(peer)
                    .map(|connection| (peer, connection))
            })
            .collect()
    }

    /// Return admitted DIDs, even if their raw transport object has already gone away.
    pub(crate) fn admitted_connection_ids(&self) -> Vec<Did> {
        self.active_peer_ids()
    }

    /// Get DIDs of active, routable connections.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.get_connections()
            .into_iter()
            .map(|(peer, _)| peer)
            .collect()
    }

    /// Disconnect a connection.
    ///
    /// Pending connections are never represented in the DHT, so cancelling one
    /// only closes its transport object. Active connections leave the DHT before
    /// the underlying WebRTC object is released.
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        if let Some(attempt) = self.pending_attempt(peer)? {
            self.cancel_pending_connection(attempt).await?;
            return Ok(());
        }

        let was_active = self.retire_active_connection(peer)?;
        if !was_active {
            self.transport
                .close_connection(&peer.to_string())
                .await
                .map_err(Error::Transport)?;
            return Ok(());
        }

        tracing::info!("removing {peer} from DHT");
        self.dht.remove(peer)?;
        self.close_connection_for_disconnect(peer).await
    }

    async fn close_connection_for_disconnect(&self, peer: Did) -> Result<()> {
        match self.transport.close_connection(&peer.to_string()).await {
            Ok(()) => Ok(()),
            Err(rings_transport::error::Error::ConnectionNotFound(_)) => {
                tracing::warn!(
                    peer = %peer,
                    "connection was already absent while disconnecting admitted peer"
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Get an active connection by DID and verify that its data channel remains open.
    /// This method will return None if the connection is not active.
    /// It can wait for a transiently disconnected active connection to recover,
    /// but pending handshakes never reach this path.
    /// See more information about [rings_transport::core::transport::WebrtcConnectionState].
    /// See also method webrtc_wait_for_data_channel_open [rings_transport::core::transport::ConnectionInterface].
    pub async fn get_and_check_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.get_and_check_connection_with_timeout(peer, DATA_CHANNEL_OPEN_TIMEOUT)
            .await
    }

    pub(crate) async fn get_and_check_connection_with_timeout(
        &self,
        peer: Did,
        wait_timeout: Duration,
    ) -> Option<SwarmConnection> {
        let conn = self.get_connection(peer)?;

        let initial_state = conn.webrtc_connection_state();
        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            state = ?initial_state,
            timeout_ms = wait_timeout.as_millis(),
            "waiting for active connection data channel"
        );

        let failure = {
            let wait_for_open = conn.connection.webrtc_wait_for_data_channel_open().fuse();
            let timeout = sleep(wait_timeout).fuse();
            pin_mut!(wait_for_open, timeout);

            select! {
                result = wait_for_open => result.err().map(|e| format!("transport_wait_failed: {e:?}")),
                _ = timeout => Some("data_channel_open_wait_timeout".to_string()),
            }
        };

        if let Some(reason) = failure {
            let final_state = conn.webrtc_connection_state();
            tracing::warn!(
                target: "rings_core::transport::data_channel",
                local = %self.dht.did,
                peer = %peer,
                initial_state = ?initial_state,
                final_state = ?final_state,
                timeout_ms = wait_timeout.as_millis(),
                reason = %reason,
                "[get_and_check_connection] connection data channel not open, will be dropped"
            );

            if let Err(e) = self.disconnect(peer).await {
                tracing::error!(
                    target: "rings_core::transport::data_channel",
                    local = %self.dht.did,
                    peer = %peer,
                    reason = %reason,
                    "failed to close connection after data-channel wait failure: {e:?}"
                );
            }

            return None;
        };

        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            state = ?conn.webrtc_connection_state(),
            "active connection data channel is open"
        );

        Some(conn)
    }
}
