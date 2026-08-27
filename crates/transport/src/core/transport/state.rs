#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Arc;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::Mutex;
#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
use std::sync::MutexGuard;

/// The state of the WebRTC connection.
/// This enum is used to define a same interface for all the platforms.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum WebrtcConnectionState {
    /// Unspecified
    #[default]
    Unspecified,

    /// WebrtcConnectionState::New indicates that any of the ICETransports or
    /// DTLSTransports are in the "new" state and none of the transports are
    /// in the "connecting", "checking", "failed" or "disconnected" state, or
    /// all transports are in the "closed" state, or there are no transports.
    New,

    /// WebrtcConnectionState::Connecting indicates that any of the
    /// ICETransports or DTLSTransports are in the "connecting" or
    /// "checking" state and none of them is in the "failed" state.
    Connecting,

    /// WebrtcConnectionState::Connected indicates that all ICETransports and
    /// DTLSTransports are in the "connected", "completed" or "closed" state
    /// and at least one of them is in the "connected" or "completed" state.
    Connected,

    /// WebrtcConnectionState::Disconnected indicates that any of the
    /// ICETransports or DTLSTransports are in the "disconnected" state
    /// and none of them are in the "failed" or "connecting" or "checking" state.
    Disconnected,

    /// WebrtcConnectionState::Failed indicates that any of the ICETransports
    /// or DTLSTransports are in a "failed" state.
    Failed,

    /// WebrtcConnectionState::Closed indicates the peer connection is closed
    /// and the isClosed member variable of PeerConnection is true.
    Closed,
}

impl WebrtcConnectionState {
    pub(crate) const fn occupies_peer_slot(self) -> bool {
        matches!(self, Self::New | Self::Connecting | Self::Connected)
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Closed)
    }
}

/// One coherent observation of the transport state used for routability.
///
/// The WebRTC and data-channel components are updated through one state cell,
/// so consumers never need to reconstruct this product from independent reads.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ConnectionStateSnapshot {
    webrtc: WebrtcConnectionState,
    data_channel_open: bool,
}

impl ConnectionStateSnapshot {
    /// Construct one transport-state observation.
    pub const fn new(webrtc: WebrtcConnectionState, data_channel_open: bool) -> Self {
        Self {
            webrtc,
            data_channel_open,
        }
    }

    /// Return the WebRTC peer-connection state.
    pub const fn webrtc(self) -> WebrtcConnectionState {
        self.webrtc
    }

    /// Return whether every outbound data channel is open.
    pub const fn data_channel_open(self) -> bool {
        self.data_channel_open
    }

    #[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
    fn apply(self, event: ConnectionStateEvent) -> Self {
        match event {
            ConnectionStateEvent::Close => Self::new(WebrtcConnectionState::Closed, false),
            ConnectionStateEvent::OutboundDataChannels(_) if self.webrtc.is_terminal() => self,
            ConnectionStateEvent::OutboundDataChannels(open) => Self::new(self.webrtc, open),
            ConnectionStateEvent::Webrtc(_) if self.webrtc == WebrtcConnectionState::Closed => self,
            ConnectionStateEvent::Webrtc(WebrtcConnectionState::Closed) => {
                Self::new(WebrtcConnectionState::Closed, false)
            }
            ConnectionStateEvent::Webrtc(_) if self.webrtc == WebrtcConnectionState::Failed => self,
            ConnectionStateEvent::Webrtc(WebrtcConnectionState::Failed) => {
                Self::new(WebrtcConnectionState::Failed, false)
            }
            ConnectionStateEvent::Webrtc(next) => Self::new(next, self.data_channel_open),
        }
    }
}

#[cfg(any(feature = "native-webrtc", feature = "web-sys-webrtc"))]
#[derive(Clone, Copy)]
enum ConnectionStateEvent {
    Webrtc(WebrtcConnectionState),
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
                WebrtcConnectionState::New,
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

    pub(crate) fn observe_webrtc(&self, state: WebrtcConnectionState) {
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
