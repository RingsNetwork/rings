use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use super::DummyConnection;
use super::DummyConnectionState;
use super::CONNS;
use crate::callback::InnerTransportCallback;
use crate::core::drop_guard::ArmedDropGuard;
use crate::core::transport::TransportConnectionState;
use crate::sync_utils::lock_recover;

/// Mark `connection_state` as closed with channels shut, returning `true` if it was not already closed.
fn mark_connection_state_closed(connection_state: &Arc<Mutex<DummyConnectionState>>) -> bool {
    let mut state = lock_recover(connection_state);
    let notify = state.webrtc != TransportConnectionState::Closed;
    state.webrtc = TransportConnectionState::Closed;
    state.data_channel_open_override = Some(false);
    notify
}

#[cfg(test)]
pub(super) fn mark_connection_state_closed_with_observer_for_test(
    connection_state: &Arc<Mutex<DummyConnectionState>>,
    before_state_gate: impl FnOnce(),
) -> bool {
    before_state_gate();
    mark_connection_state_closed(connection_state)
}

/// Shared teardown handles for retiring a [`DummyConnection`] generation safely.
#[derive(Clone)]
pub(super) struct DummyRetirementFence {
    /// Random identifier of the local dummy connection in `CONNS`.
    rand_id: String,
    /// Connection callback notified on terminal close.
    callback: Arc<InnerTransportCallback>,
    /// Abort handle for the connection's background event listener task.
    listener: tokio::task::AbortHandle,
    /// Optional peer dummy connection identifier paired with this connection.
    remote_rand_id: Arc<Mutex<Option<String>>>,
    /// Shared dummy connection phase and channel override state.
    connection_state: Arc<Mutex<DummyConnectionState>>,
    /// Atomic gate controlling whether the connection still accepts incoming events.
    accepting_events: Arc<AtomicBool>,
    /// Watch sender signaled once physical close cleanup finishes.
    physical_close_completed: Arc<tokio::sync::watch::Sender<bool>>,
    /// Tokio runtime handle used to spawn close notifications.
    runtime: tokio::runtime::Handle,
}

/// RAII guard ensuring dummy connection retirement finishes even on cancellation or drop.
pub(super) struct DummyRetirementGuard {
    /// One-shot drop guard executing [`finish_dummy_retirement`].
    cleanup: ArmedDropGuard<DummyRetirement, fn(DummyRetirement)>,
}

/// Payload carried by [`DummyRetirementGuard`] to complete teardown.
struct DummyRetirement {
    /// Retirement fence performing teardown.
    fence: DummyRetirementFence,
    /// Whether this transition was the first to close the connection and should emit callbacks.
    notify: bool,
}

/// Execute teardown for `retirement` when fired or dropped.
fn finish_dummy_retirement(retirement: DummyRetirement) {
    retirement.fence.finish_retirement(retirement.notify);
}

impl DummyRetirementFence {
    /// Capture teardown handles from `connection`.
    pub(super) fn new(connection: &DummyConnection) -> Self {
        Self {
            rand_id: connection.rand_id.clone(),
            callback: Arc::clone(&connection.callback),
            listener: connection.event_listener.abort_handle(),
            remote_rand_id: Arc::clone(&connection.remote_rand_id),
            connection_state: Arc::clone(&connection.connection_state),
            accepting_events: Arc::clone(&connection.accepting_events),
            physical_close_completed: Arc::clone(&connection.physical_close_completed),
            runtime: connection.retirement_runtime.clone(),
        }
    }

    /// Mark the connection closed and return a [`DummyRetirementGuard`] to complete cleanup.
    pub(super) fn begin(self) -> DummyRetirementGuard {
        let notify = self.mark_closed();
        DummyRetirementGuard {
            cleanup: ArmedDropGuard::new(
                DummyRetirement {
                    fence: self,
                    notify,
                },
                finish_dummy_retirement,
            ),
        }
    }

    /// Transition the connection state to `Closed`, returning `true` if it was not already closed.
    pub(super) fn mark_closed(&self) -> bool {
        mark_connection_state_closed(&self.connection_state)
    }

    /// Stop accepting events, deregister the connection, abort its listener, and emit close callbacks.
    pub(super) fn finish_retirement(&self, notify: bool) {
        let remote = lock_recover(&self.remote_rand_id)
            .as_ref()
            .and_then(|remote| CONNS.get(remote).map(|connection| connection.clone()));
        self.accepting_events.store(false, Ordering::Release);
        CONNS.remove(&self.rand_id);
        self.listener.abort();
        self.physical_close_completed.send_replace(true);
        if !notify {
            return;
        }
        let callback = Arc::clone(&self.callback);
        self.runtime.spawn(async move {
            callback
                .on_connection_state_change(TransportConnectionState::Closed)
                .await;
            callback.on_channel_close().await;
            if let Some(remote) = remote {
                remote
                    .set_connection_state(TransportConnectionState::Disconnected)
                    .await;
                remote
                    .set_connection_state(TransportConnectionState::Closed)
                    .await;
            }
        });
    }

    /// Synchronously mark closed and execute full retirement teardown.
    pub(super) fn request(&self) {
        let notify = self.mark_closed();
        self.finish_retirement(notify);
    }
}

impl DummyRetirementGuard {
    /// Complete dummy connection retirement immediately and disarm the drop guard.
    pub(super) fn finish(mut self) {
        self.cleanup.fire();
    }
}
