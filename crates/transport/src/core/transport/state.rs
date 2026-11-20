#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Arc;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Mutex;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::MutexGuard;

/// Lifecycle state of a transport connection.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum TransportConnectionState {
    /// Unspecified
    #[default]
    Unspecified,

    /// The connection is newly created and has not begun active negotiation or checking.
    New,

    /// The connection is actively negotiating or checking connectivity.
    Connecting,

    /// The connection has established a usable transport path.
    Connected,

    /// The connection has transiently lost connectivity and may recover or fail.
    Disconnected,

    /// The connection has permanently failed.
    Failed,

    /// The connection has been closed.
    Closed,
}

impl TransportConnectionState {
    /// Return whether a connection in this state occupies its peer slot in the pool.
    pub const fn occupies_peer_slot(self) -> bool {
        matches!(self, Self::New | Self::Connecting | Self::Connected)
    }

    /// Whether the connection generation has ended: it fails or closes, and never reconnects.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Closed)
    }
}

/// One coherent observation of the transport state used for routability.
///
/// The connection phase and channel readiness components are updated through one state cell,
/// so consumers never need to reconstruct this product from independent reads.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ConnectionStateSnapshot {
    state: TransportConnectionState,
    channel_open: bool,
}

impl ConnectionStateSnapshot {
    /// Construct one transport-state observation.
    pub const fn new(state: TransportConnectionState, channel_open: bool) -> Self {
        Self {
            state,
            channel_open,
        }
    }

    /// Return the transport connection phase.
    pub const fn state(self) -> TransportConnectionState {
        self.state
    }

    /// Return whether every outbound payload channel is open.
    pub const fn channel_open(self) -> bool {
        self.channel_open
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    fn apply(self, event: ConnectionStateEvent) -> Self {
        match event {
            ConnectionStateEvent::Close => Self::new(TransportConnectionState::Closed, false),
            ConnectionStateEvent::OutboundDataChannels(_) if self.state.is_terminal() => self,
            ConnectionStateEvent::OutboundDataChannels(open) => Self::new(self.state, open),
            ConnectionStateEvent::Webrtc(_) if self.state == TransportConnectionState::Closed => {
                self
            }
            ConnectionStateEvent::Webrtc(TransportConnectionState::Closed) => {
                Self::new(TransportConnectionState::Closed, false)
            }
            ConnectionStateEvent::Webrtc(_) if self.state == TransportConnectionState::Failed => {
                self
            }
            ConnectionStateEvent::Webrtc(TransportConnectionState::Failed) => {
                Self::new(TransportConnectionState::Failed, false)
            }
            ConnectionStateEvent::Webrtc(next) => Self::new(next, self.channel_open),
        }
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Clone, Copy)]
enum ConnectionStateEvent {
    Webrtc(TransportConnectionState),
    OutboundDataChannels(bool),
    Close,
}

/// Shared event-maintained transport state for concrete backends.
///
/// Clone law: every clone references the same state cell; cloning does not
/// duplicate protocol state. `Closed` is absorbing, and terminal states can
/// never retain or regain an open data channel.
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Clone)]
pub(crate) struct ConnectionStateCell {
    state: Arc<Mutex<ConnectionStateSnapshot>>,
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
impl ConnectionStateCell {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionStateSnapshot::new(
                TransportConnectionState::New,
                false,
            ))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ConnectionStateSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn apply(&self, event: ConnectionStateEvent) {
        let mut state = self.lock();
        *state = state.apply(event);
    }

    pub(crate) fn observe_webrtc(&self, state: TransportConnectionState) {
        self.apply(ConnectionStateEvent::Webrtc(state));
    }

    pub(crate) fn observe_outbound_data_channels(&self, open: bool) {
        self.apply(ConnectionStateEvent::OutboundDataChannels(open));
    }

    pub(crate) fn close(&self) {
        self.apply(ConnectionStateEvent::Close);
    }

    pub(crate) fn snapshot(&self) -> ConnectionStateSnapshot {
        *self.lock()
    }
}
