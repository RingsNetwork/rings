use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use super::DummyConnection;
use super::DummyConnectionState;
use super::CONNS;
use crate::callback::InnerTransportCallback;
use crate::core::drop_guard::ArmedDropGuard;
use crate::core::transport::WebrtcConnectionState;
use crate::sync_utils::lock_recover;

fn mark_connection_state_closed(connection_state: &Arc<Mutex<DummyConnectionState>>) -> bool {
    let mut state = lock_recover(connection_state);
    let notify = state.webrtc != WebrtcConnectionState::Closed;
    state.webrtc = WebrtcConnectionState::Closed;
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

#[derive(Clone)]
pub(super) struct DummyRetirementFence {
    rand_id: String,
    callback: Arc<InnerTransportCallback>,
    listener: tokio::task::AbortHandle,
    remote_rand_id: Arc<Mutex<Option<String>>>,
    connection_state: Arc<Mutex<DummyConnectionState>>,
    accepting_events: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
}

pub(super) struct DummyRetirementGuard {
    cleanup: ArmedDropGuard<DummyRetirement, fn(DummyRetirement)>,
}

struct DummyRetirement {
    fence: DummyRetirementFence,
    notify: bool,
}

fn finish_dummy_retirement(retirement: DummyRetirement) {
    retirement.fence.finish_retirement(retirement.notify);
}

impl DummyRetirementFence {
    pub(super) fn new(connection: &DummyConnection) -> Self {
        Self {
            rand_id: connection.rand_id.clone(),
            callback: Arc::clone(&connection.callback),
            listener: connection.event_listener.abort_handle(),
            remote_rand_id: Arc::clone(&connection.remote_rand_id),
            connection_state: Arc::clone(&connection.connection_state),
            accepting_events: Arc::clone(&connection.accepting_events),
            runtime: connection.retirement_runtime.clone(),
        }
    }

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

    pub(super) fn mark_closed(&self) -> bool {
        mark_connection_state_closed(&self.connection_state)
    }

    pub(super) fn finish_retirement(&self, notify: bool) {
        let remote = lock_recover(&self.remote_rand_id)
            .as_ref()
            .and_then(|remote| CONNS.get(remote).map(|connection| connection.clone()));
        self.accepting_events.store(false, Ordering::Release);
        CONNS.remove(&self.rand_id);
        self.listener.abort();
        if !notify {
            return;
        }
        let callback = Arc::clone(&self.callback);
        self.runtime.spawn(async move {
            callback
                .on_peer_connection_state_change(WebrtcConnectionState::Closed)
                .await;
            callback.on_data_channel_close().await;
            if let Some(remote) = remote {
                remote
                    .set_webrtc_connection_state(WebrtcConnectionState::Disconnected)
                    .await;
                remote
                    .set_webrtc_connection_state(WebrtcConnectionState::Closed)
                    .await;
            }
        });
    }

    pub(super) fn request(&self) {
        let notify = self.mark_closed();
        self.finish_retirement(notify);
    }
}

impl DummyRetirementGuard {
    pub(super) fn finish(mut self) {
        self.cleanup.fire();
    }
}
