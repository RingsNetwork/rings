use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::ConnectionStateSnapshot;
use rings_transport::core::transport::TransportConnectionState;

use super::SwarmConnection;
use crate::error::Error;
use crate::error::Result;

/// The transport-level ability of an admitted or admitting connection to make
/// forward progress.
///
/// This is the only interpretation of the WebRTC/data-channel product state
/// used by admission, failover, and data-plane delivery. In particular, an
/// `Open` data channel does not make a transiently `Disconnected` ICE
/// transport ready.
///
/// Invariant: every value is classified from one coherent transport-layer
/// snapshot. The two components are never reconstructed from independent
/// observations in core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransportReadiness(TransportReadinessState);

/// Classified transport readiness state pairing a phase with its coherent snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportReadinessState {
    /// Transport is connecting or connected and the data channel is open.
    Ready(ConnectionStateSnapshot),
    /// Transport is connecting or connected, waiting for the data channel to open.
    AwaitingChannel(ConnectionStateSnapshot),
    /// Transport is not yet connected or is transiently disconnected.
    Recovering(ConnectionStateSnapshot),
    /// Transport has reached a terminal failed or closed state.
    Terminal(ConnectionStateSnapshot),
}

impl TransportReadiness {
    /// Classify one coherent WebRTC/data-channel product-state observation.
    const fn from_snapshot(snapshot: ConnectionStateSnapshot) -> Self {
        Self(match snapshot.state() {
            TransportConnectionState::Connecting | TransportConnectionState::Connected
                if snapshot.channel_open() =>
            {
                TransportReadinessState::Ready(snapshot)
            }
            TransportConnectionState::Connecting | TransportConnectionState::Connected => {
                TransportReadinessState::AwaitingChannel(snapshot)
            }
            TransportConnectionState::Unspecified
            | TransportConnectionState::New
            | TransportConnectionState::Disconnected => {
                TransportReadinessState::Recovering(snapshot)
            }
            TransportConnectionState::Failed | TransportConnectionState::Closed => {
                TransportReadinessState::Terminal(snapshot)
            }
        })
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(super) const fn for_test(state: TransportConnectionState, channel_open: bool) -> Self {
        Self::from_snapshot(ConnectionStateSnapshot::new(state, channel_open))
    }

    /// Return whether the connection is ready to make forward progress.
    pub(crate) const fn can_make_progress(self) -> bool {
        matches!(self.0, TransportReadinessState::Ready(_))
    }

    /// Return whether the connection has reached a terminal failed or closed state.
    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self.0, TransportReadinessState::Terminal(_))
    }

    /// Return the underlying coherent connection state snapshot.
    const fn snapshot(self) -> ConnectionStateSnapshot {
        match self.0 {
            TransportReadinessState::Ready(snapshot)
            | TransportReadinessState::AwaitingChannel(snapshot)
            | TransportReadinessState::Recovering(snapshot)
            | TransportReadinessState::Terminal(snapshot) => snapshot,
        }
    }

    /// Return the underlying WebRTC/transport connection state.
    pub(crate) const fn state(self) -> TransportConnectionState {
        self.snapshot().state()
    }

    /// Return whether the data channel was open in this snapshot.
    pub(crate) const fn channel_open(self) -> bool {
        self.snapshot().channel_open()
    }

    /// Return a short snake-case label describing this readiness state.
    pub(crate) const fn as_str(self) -> &'static str {
        match self.0 {
            TransportReadinessState::Ready(_) => "ready",
            TransportReadinessState::AwaitingChannel(_) => "awaiting_channel",
            TransportReadinessState::Recovering(_) => "recovering",
            TransportReadinessState::Terminal(_) => "terminal",
        }
    }

    /// Succeed if ready to make progress, or return `Error::TransportNotReady`.
    pub(crate) fn ensure_can_make_progress(self) -> Result<()> {
        if self.can_make_progress() {
            return Ok(());
        }
        Err(Error::TransportNotReady {
            state: self.state(),
            channel_open: self.channel_open(),
        })
    }
}

impl SwarmConnection {
    /// Snapshot and classify the current transport readiness of this connection.
    pub(crate) fn readiness(&self) -> TransportReadiness {
        TransportReadiness::from_snapshot(self.connection.connection_state_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn snapshot(
        state: TransportConnectionState,
        channel_open: bool,
    ) -> ConnectionStateSnapshot {
        ConnectionStateSnapshot::new(state, channel_open)
    }

    #[test]
    fn test_transport_readiness_classifies_the_complete_product_state() {
        for state in [
            TransportConnectionState::Unspecified,
            TransportConnectionState::New,
            TransportConnectionState::Disconnected,
        ] {
            for channel_open in [false, true] {
                let snapshot = snapshot(state, channel_open);
                assert_eq!(
                    TransportReadiness::from_snapshot(snapshot),
                    TransportReadiness(TransportReadinessState::Recovering(snapshot))
                );
            }
        }

        for state in [
            TransportConnectionState::Connecting,
            TransportConnectionState::Connected,
        ] {
            let closed = snapshot(state, false);
            assert_eq!(
                TransportReadiness::from_snapshot(closed),
                TransportReadiness(TransportReadinessState::AwaitingChannel(closed))
            );
            let open = snapshot(state, true);
            assert_eq!(
                TransportReadiness::from_snapshot(open),
                TransportReadiness(TransportReadinessState::Ready(open))
            );
        }

        for state in [
            TransportConnectionState::Failed,
            TransportConnectionState::Closed,
        ] {
            for channel_open in [false, true] {
                let snapshot = snapshot(state, channel_open);
                assert_eq!(
                    TransportReadiness::from_snapshot(snapshot),
                    TransportReadiness(TransportReadinessState::Terminal(snapshot))
                );
            }
        }
    }

    #[test]
    fn test_only_ready_transport_can_make_progress() {
        for state in [
            TransportConnectionState::Unspecified,
            TransportConnectionState::New,
            TransportConnectionState::Disconnected,
            TransportConnectionState::Failed,
            TransportConnectionState::Closed,
        ] {
            assert!(
                !TransportReadiness::from_snapshot(snapshot(state, true)).can_make_progress(),
                "{state:?}"
            );
        }
        for state in [
            TransportConnectionState::Connecting,
            TransportConnectionState::Connected,
        ] {
            assert!(
                TransportReadiness::from_snapshot(snapshot(state, true)).can_make_progress(),
                "{state:?}"
            );
            assert!(
                !TransportReadiness::from_snapshot(snapshot(state, false)).can_make_progress(),
                "{state:?}"
            );
        }
    }

    #[test]
    fn test_only_terminal_readiness_errors_degrade_peer_quality() {
        assert!(!Error::TransportNotReady {
            state: TransportConnectionState::Disconnected,
            channel_open: true,
        }
        .records_peer_send_failure());
        assert!(Error::TransportNotReady {
            state: TransportConnectionState::Failed,
            channel_open: true,
        }
        .records_peer_send_failure());
        assert!(!Error::ConnectionAttemptSuperseded {
            peer: crate::ecc::SecretKey::random().address().into(),
            generation: 1,
        }
        .records_peer_send_failure());
        assert!(
            !Error::Transport(rings_transport::error::Error::SendPermitRevoked)
                .records_peer_send_failure()
        );
        let invariant = Error::CancelledDetachedAdmissionPublishedSuccess;
        assert!(!invariant.is_deferrable_data_plane_send());
        assert!(!invariant.records_peer_send_failure());
    }

    #[test]
    fn test_pre_acceptance_backpressure_is_deferrable_and_never_degrades_peer_quality() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let backpressure = [
            Error::ChannelSendQueueTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::OutboundTransferCapacityExceeded { peer, capacity: 1 },
            Error::OutboundTransferMemoryCapacityExceeded {
                peer,
                requested_bytes: 1,
                capacity_bytes: 1,
            },
            Error::OutboundTransferAdmissionTimeout {
                peer,
                timeout_ms: 1,
            },
            Error::OutboundFirstFrameAdmissionTimeout {
                peer,
                timeout_ms: 1,
            },
        ];

        for error in backpressure {
            assert!(error.is_local_send_backpressure(), "{error:?}");
            assert!(error.is_deferrable_data_plane_send(), "{error:?}");
            assert!(!error.records_peer_send_failure(), "{error:?}");
        }
    }

    #[test]
    fn test_post_acceptance_timeouts_are_ambiguous_and_not_retryable() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let ambiguous = [
            Error::ChannelSendCompletionTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::ChannelDeliveryTimeout {
                peer,
                timeout_ms: 1,
                context: "test",
            },
            Error::DetachedPayloadCleanupTimeout {
                peer,
                timeout_ms: 1,
            },
            Error::TrackedPayloadCleanupTimeout {
                peer,
                timeout_ms: 1,
            },
        ];

        for error in ambiguous {
            assert!(!error.is_local_send_backpressure(), "{error:?}");
            assert!(!error.is_deferrable_data_plane_send(), "{error:?}");
            assert!(!error.records_peer_send_failure(), "{error:?}");
        }
    }

    #[test]
    fn test_data_plane_deferral_errors_require_fresh_topology() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let deferrals = [
            Error::ChannelSendQueueTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::ConnectionAttemptSuperseded {
                peer,
                generation: 1,
            },
            Error::TransportNotReady {
                state: TransportConnectionState::Disconnected,
                channel_open: true,
            },
            Error::SwarmMissDidInTable(peer),
            Error::Transport(rings_transport::error::Error::SendPermitRevoked),
        ];

        for error in deferrals {
            assert!(error.is_deferrable_data_plane_send(), "{error:?}");
        }
        assert!(!Error::InvalidMessage("invalid".to_string()).is_deferrable_data_plane_send());
    }
}
