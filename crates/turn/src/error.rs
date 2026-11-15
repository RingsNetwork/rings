use rings_core::dht::Did;

/// Error type for [`crate::TurnServer`] and [`crate::DelegationAuthHandler`].
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// Failed to bind or query the local UDP socket.
    #[error("UDP socket I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Underlying `turn` crate protocol or server error.
    #[error("TURN server error: {0}")]
    Turn(#[from] turn::Error),

    /// Underlying `webrtc-util` connection error.
    #[error("WebRTC util connection error: {0}")]
    Util(#[from] webrtc_util::Error),

    /// Delegation or signature verification error from `rings-core`.
    #[error("Delegation verification error: {0}")]
    Core(#[from] rings_core::error::Error),

    /// The authenticated delegator DID is not admitted by the server's [`crate::TurnAdmissionPolicy`].
    #[error("Delegator DID {0} is not admitted by TURN policy")]
    DelegatorNotAdmitted(Did),

    /// Relay port range configuration is invalid.
    #[error("Invalid relay port range: min_port={min_port}, max_port={max_port}")]
    InvalidPortRange {
        /// Configured minimum relay port.
        min_port: u16,
        /// Configured maximum relay port.
        max_port: u16,
    },
}

/// Result alias for `rings-turn`.
pub type Result<T> = std::result::Result<T, TurnError>;
