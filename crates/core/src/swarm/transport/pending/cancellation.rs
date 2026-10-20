//! Cancellation and expiry of unadmitted generations: the transitions that release a pending
//! or admitting record, with its signaling state, and close its transport object.

use rings_transport::core::transport::TransportInterface;

use super::Handshake;
use super::PendingConnectionAttempt;
use super::SwarmConnection;
use super::SwarmTransport;
use super::PENDING_CONNECTION_TIMEOUT_MS;
use crate::error::Error;
use crate::error::Result;
use crate::utils::get_epoch_ms_i64;

/// Which unadmitted phases a cancellation may release, in the registry's vocabulary.
enum CancellationScope {
    /// `Pending` only: no data channel has opened, so only the record and its transport object
    /// are released.
    Pending,
    /// `Pending` or `Admitting`.
    Unadmitted,
}

struct RetiredPendingConnection {
    /// Raw transport released by retirement, if it still exists in the backend.
    connection: Option<SwarmConnection>,
}

impl SwarmTransport {
    /// Release the record of `attempt` iff it is still pending, leaving its transport object,
    /// if any, in place: the `Pending` arm of the one cancellation transition without the close.
    pub(in crate::swarm::transport) fn retire_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.retire_pending_connection_for_close(attempt, CancellationScope::Pending)
            .map(|retired| retired.is_some())
    }

    /// Cancel a current pending or admitting handshake and release its transport object.
    pub(crate) async fn cancel_unadmitted_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.cancel_connection(attempt, CancellationScope::Unadmitted)
            .await
    }

    /// Yield `attempt` to a crossing offer iff its signaling state is still `observed`, the
    /// state the verdict was decided on: its record is replaced by the peer's answering
    /// generation in one lifecycle transition (`Law (Atomic yield)`), then its transport object
    /// is closed. Returns the answering generation, or `None` when the attempt moved on (or was
    /// released) meanwhile.
    pub(in crate::swarm::transport) async fn yield_pending_in(
        &self,
        attempt: PendingConnectionAttempt,
        observed: Handshake,
    ) -> Result<Option<PendingConnectionAttempt>> {
        let (answering, connection) = {
            let _lifecycle = self.connection_lifecycle()?;
            let answering = self.peer_lifecycles()?.replace_pending_in(
                attempt,
                &observed,
                get_epoch_ms_i64(),
            )?;
            let Some(answering) = answering else {
                return Ok(None);
            };
            self.cancel_pending_finger_updates(attempt)?;
            (answering, self.get_raw_connection(attempt.peer))
        };
        tracing::debug!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            answering = answering.generation,
            "pending connection yielded to a crossing offer"
        );
        // The answering generation has no transport yet, so the raw connection is the yielded one.
        if let Some(connection) = connection {
            self.transport
                .close_connection_if_current(&connection.connection)
                .await
                .map_err(Error::Transport)?;
        }
        Ok(Some(answering))
    }

    /// Cancel `attempt` iff it is still pending, that is, no data channel has opened on it, and
    /// release its transport object. An admitting generation has a data channel open and its
    /// DHT admission in flight; it is left to complete, or to fail on its own path.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        self.cancel_connection(attempt, CancellationScope::Pending)
            .await
    }

    /// Release the unadmitted record of `attempt` within `scope` and close its transport
    /// object; `false` when `attempt` owns no record in that scope.
    async fn cancel_connection(
        &self,
        attempt: PendingConnectionAttempt,
        scope: CancellationScope,
    ) -> Result<bool> {
        let Some(retired) = self.retire_pending_connection_for_close(attempt, scope)? else {
            return Ok(false);
        };
        tracing::debug!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %attempt.peer,
            generation = attempt.generation,
            "pending connection cancelled"
        );
        if let Some(connection) = retired.connection {
            self.transport
                .close_connection_if_current(&connection.connection)
                .await
                .map_err(Error::Transport)?;
        }
        Ok(true)
    }

    fn retire_pending_connection_for_close(
        &self,
        attempt: PendingConnectionAttempt,
        scope: CancellationScope,
    ) -> Result<Option<RetiredPendingConnection>> {
        let _lifecycle = self.connection_lifecycle()?;
        let removed = {
            let mut lifecycles = self.peer_lifecycles()?;
            match scope {
                CancellationScope::Pending => lifecycles.remove_pending(attempt),
                CancellationScope::Unadmitted => lifecycles.remove_unadmitted(attempt),
            }
        };
        if !removed {
            return Ok(None);
        }
        self.cancel_pending_finger_updates(attempt)?;
        Ok(Some(RetiredPendingConnection {
            connection: self.get_raw_connection(attempt.peer),
        }))
    }

    pub(crate) async fn abandon_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        operation: &str,
    ) {
        if let Err(error) = self.cancel_unadmitted_connection(attempt).await {
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
        let expired = {
            let _lifecycle = self.connection_lifecycle()?;
            let expired = self.peer_lifecycles()?.expire(get_epoch_ms_i64());
            expired
                .into_iter()
                .map(|expired| {
                    self.cancel_pending_finger_updates(expired.attempt)?;
                    let connection = self.get_raw_connection(expired.attempt.peer);
                    Ok((expired, connection))
                })
                .collect::<Result<Vec<_>>>()?
        };
        for (expired, connection) in expired {
            let attempt = expired.attempt;
            let state = connection
                .as_ref()
                .map(|connection| connection.webrtc_connection_state());
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %attempt.peer,
                generation = attempt.generation,
                age_ms = expired.age_ms,
                timeout_ms = PENDING_CONNECTION_TIMEOUT_MS,
                phase = expired.phase.as_str(),
                state = ?state,
                "connection attempt timed out before admission commit"
            );
            if let Some(connection) = connection {
                self.transport
                    .close_connection_if_current(&connection.connection)
                    .await
                    .map_err(Error::Transport)?;
            }
        }
        Ok(())
    }
}
