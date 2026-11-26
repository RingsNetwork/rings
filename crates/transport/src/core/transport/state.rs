use std::sync::Arc;
use std::sync::Mutex;
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
    /// Current transport connection phase.
    state: TransportConnectionState,
    /// Whether all outbound payload data channels are currently open.
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

    /// Apply a single state transition event to this snapshot.
    fn apply(self, event: ConnectionStateEvent) -> Self {
        match event {
            ConnectionStateEvent::Close => Self::new(TransportConnectionState::Closed, false),
            ConnectionStateEvent::Channels(_) if self.state.is_terminal() => self,
            ConnectionStateEvent::Channels(open) => Self::new(self.state, open),
            ConnectionStateEvent::Phase(_) if self.state == TransportConnectionState::Closed => {
                self
            }
            ConnectionStateEvent::Phase(TransportConnectionState::Closed) => {
                Self::new(TransportConnectionState::Closed, false)
            }
            ConnectionStateEvent::Phase(_) if self.state == TransportConnectionState::Failed => {
                self
            }
            ConnectionStateEvent::Phase(TransportConnectionState::Failed) => {
                Self::new(TransportConnectionState::Failed, false)
            }
            ConnectionStateEvent::Phase(next) => Self::new(next, self.channel_open),
        }
    }
}

/// Internal transition event applied to [`ConnectionStateSnapshot`].
#[derive(Clone, Copy)]
enum ConnectionStateEvent {
    /// Connection phase changed to the given state.
    Phase(TransportConnectionState),
    /// Outbound payload channel readiness changed.
    Channels(bool),
    /// Local close requested; permanently transitions to `(Closed, false)`.
    Close,
}

/// Shared event-maintained transport product-state cell for concrete connection backends.
///
/// Clone law: every clone references the same state cell; cloning does not
/// duplicate protocol state. `Closed` is absorbing, and terminal states can
/// never retain or regain open payload channels.
#[derive(Clone, Debug)]
pub struct ConnectionStateCell {
    /// Synchronized product state of connection phase and channel readiness.
    state: Arc<Mutex<ConnectionStateSnapshot>>,
}

impl Default for ConnectionStateCell {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionStateCell {
    /// Construct a new [`ConnectionStateCell`] in `(New, false)`.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ConnectionStateSnapshot::new(
                TransportConnectionState::New,
                false,
            ))),
        }
    }

    /// Lock the inner snapshot, recovering from mutex poisoning.
    fn lock(&self) -> MutexGuard<'_, ConnectionStateSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Atomically apply `event` to the stored snapshot.
    fn apply(&self, event: ConnectionStateEvent) {
        let mut state = self.lock();
        *state = state.apply(event);
    }

    /// Record a connection-phase transition on this cell.
    pub fn observe_phase(&self, state: TransportConnectionState) {
        self.apply(ConnectionStateEvent::Phase(state));
    }

    /// Record whether all outbound payload channels are open (`true`) or closed (`false`).
    pub fn observe_channels(&self, open: bool) {
        self.apply(ConnectionStateEvent::Channels(open));
    }

    /// Permanently transition this cell to `(Closed, false)`.
    pub fn close(&self) {
        self.apply(ConnectionStateEvent::Close);
    }

    /// Read one atomic `(state, channel_open)` snapshot.
    pub fn snapshot(&self) -> ConnectionStateSnapshot {
        *self.lock()
    }
}
