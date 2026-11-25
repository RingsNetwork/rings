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

    /// Underlying `stun` crate message encoding or decoding error.
    #[error("STUN protocol error: {0}")]
    Stun(#[from] stun::Error),

    /// Unclaimed session reservations reached `max_reservations`.
    #[error("TURN session reservation cap reached (max_reservations={max_reservations})")]
    ReservationCapacityReached {
        /// Configured maximum concurrent unclaimed TURN session reservations.
        max_reservations: usize,
    },

    /// Active allocations plus unclaimed session reservations reached `max_allocations`.
    #[error("TURN allocation capacity reached (max_allocations={max_allocations})")]
    AllocationCapacityReached {
        /// Configured maximum concurrent TURN allocations.
        max_allocations: usize,
    },

    /// Session reservation TTL must be non-zero.
    #[error("TURN session reservation TTL must be greater than zero")]
    InvalidReservationTtl,

    /// Underlying `webrtc-util` connection error.
    #[error("WebRTC util connection error: {0}")]
    Util(#[from] webrtc_util::Error),

    /// Delegation or signature verification error from `rings-core`.
    #[error("Delegation verification error: {0}")]
    Core(#[from] rings_core::error::Error),

    /// The authenticated delegator DID is not admitted by the server's [`crate::TurnAdmissionPolicy`].
    #[error("Delegator DID {0} is not admitted by TURN policy")]
    DelegatorNotAdmitted(Did),

    /// Request username does not match the established allocation owner.
    #[error("TURN request username does not match established allocation owner")]
    AllocationOwnerMismatch,

    /// Relay port range configuration is invalid.
    #[error("Invalid relay port range: min_port={min_port}, max_port={max_port}")]
    InvalidPortRange {
        /// Configured minimum relay port.
        min_port: u16,
        /// Configured maximum relay port.
        max_port: u16,
    },

    /// Configured advertised `relay_ip` and local `bind_ip` belong to different address families.
    #[error("Relay address family mismatch: relay_ip={relay_ip}, bind_ip={bind_ip}")]
    RelayAddressFamilyMismatch {
        /// Configured external/advertised relay IP.
        relay_ip: std::net::IpAddr,
        /// Configured local relay socket bind IP.
        bind_ip: std::net::IpAddr,
    },

    /// Configured advertised listener `advertised_ip` and local `listen_ip` belong to different address families.
    #[error(
        "Listener address family mismatch: advertised_ip={advertised_ip}, listen_ip={listen_ip}"
    )]
    ListenerAddressFamilyMismatch {
        /// Configured external/advertised listener IP.
        advertised_ip: std::net::IpAddr,
        /// Configured local listener bind IP.
        listen_ip: std::net::IpAddr,
    },
}

/// Result alias for `rings-turn`.
pub type Result<T> = std::result::Result<T, TurnError>;
