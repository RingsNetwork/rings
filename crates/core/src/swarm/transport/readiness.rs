use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::ConnectionStateSnapshot;
use rings_transport::core::transport::WebrtcConnectionState;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportReadinessState {
    Ready(ConnectionStateSnapshot),
    AwaitingDataChannel(ConnectionStateSnapshot),
    Recovering(ConnectionStateSnapshot),
    Terminal(ConnectionStateSnapshot),
}

impl TransportReadiness {
    /// Classify one coherent WebRTC/data-channel product-state observation.
    const fn from_snapshot(snapshot: ConnectionStateSnapshot) -> Self {
        Self(match snapshot.webrtc() {
            WebrtcConnectionState::Connecting | WebrtcConnectionState::Connected
                if snapshot.data_channel_open() =>
            {
                TransportReadinessState::Ready(snapshot)
            }
            WebrtcConnectionState::Connecting | WebrtcConnectionState::Connected => {
                TransportReadinessState::AwaitingDataChannel(snapshot)
            }
            WebrtcConnectionState::Unspecified
            | WebrtcConnectionState::New
            | WebrtcConnectionState::Disconnected => TransportReadinessState::Recovering(snapshot),
            WebrtcConnectionState::Failed | WebrtcConnectionState::Closed => {
                TransportReadinessState::Terminal(snapshot)
            }
        })
    }

    pub(crate) const fn can_make_progress(self) -> bool {
        matches!(self.0, TransportReadinessState::Ready(_))
    }

    pub(crate) const fn is_terminal(self) -> bool {
        matches!(self.0, TransportReadinessState::Terminal(_))
    }

    const fn snapshot(self) -> ConnectionStateSnapshot {
        match self.0 {
            TransportReadinessState::Ready(snapshot)
            | TransportReadinessState::AwaitingDataChannel(snapshot)
            | TransportReadinessState::Recovering(snapshot)
            | TransportReadinessState::Terminal(snapshot) => snapshot,
        }
    }

    pub(crate) const fn state(self) -> WebrtcConnectionState {
        self.snapshot().webrtc()
    }

    pub(crate) const fn data_channel_open(self) -> bool {
        self.snapshot().data_channel_open()
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self.0 {
            TransportReadinessState::Ready(_) => "ready",
            TransportReadinessState::AwaitingDataChannel(_) => "awaiting_data_channel",
            TransportReadinessState::Recovering(_) => "recovering",
            TransportReadinessState::Terminal(_) => "terminal",
        }
    }

    pub(crate) fn ensure_can_make_progress(self) -> Result<()> {
        if self.can_make_progress() {
            return Ok(());
        }
        Err(Error::TransportNotReady {
            state: self.state(),
            data_channel_open: self.data_channel_open(),
        })
    }
}

impl SwarmConnection {
    pub(crate) fn readiness(&self) -> TransportReadiness {
        TransportReadiness::from_snapshot(self.connection.connection_state_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn snapshot(
        state: WebrtcConnectionState,
        data_channel_open: bool,
    ) -> ConnectionStateSnapshot {
        ConnectionStateSnapshot::new(state, data_channel_open)
    }

    #[test]
    fn transport_readiness_classifies_the_complete_product_state() {
        for state in [
            WebrtcConnectionState::Unspecified,
            WebrtcConnectionState::New,
            WebrtcConnectionState::Disconnected,
        ] {
            for data_channel_open in [false, true] {
                let snapshot = snapshot(state, data_channel_open);
                assert_eq!(
                    TransportReadiness::from_snapshot(snapshot),
                    TransportReadiness(TransportReadinessState::Recovering(snapshot))
                );
            }
        }

        for state in [
            WebrtcConnectionState::Connecting,
            WebrtcConnectionState::Connected,
        ] {
            let closed = snapshot(state, false);
            assert_eq!(
                TransportReadiness::from_snapshot(closed),
                TransportReadiness(TransportReadinessState::AwaitingDataChannel(closed))
            );
            let open = snapshot(state, true);
            assert_eq!(
                TransportReadiness::from_snapshot(open),
                TransportReadiness(TransportReadinessState::Ready(open))
            );
        }

        for state in [WebrtcConnectionState::Failed, WebrtcConnectionState::Closed] {
            for data_channel_open in [false, true] {
                let snapshot = snapshot(state, data_channel_open);
                assert_eq!(
                    TransportReadiness::from_snapshot(snapshot),
                    TransportReadiness(TransportReadinessState::Terminal(snapshot))
                );
            }
        }
    }

    #[test]
    fn only_ready_transport_can_make_progress() {
        for state in [
            WebrtcConnectionState::Unspecified,
            WebrtcConnectionState::New,
            WebrtcConnectionState::Disconnected,
            WebrtcConnectionState::Failed,
            WebrtcConnectionState::Closed,
        ] {
            assert!(
                !TransportReadiness::from_snapshot(snapshot(state, true)).can_make_progress(),
                "{state:?}"
            );
        }
        for state in [
            WebrtcConnectionState::Connecting,
            WebrtcConnectionState::Connected,
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
    fn only_terminal_readiness_errors_degrade_peer_quality() {
        assert!(!Error::TransportNotReady {
            state: WebrtcConnectionState::Disconnected,
            data_channel_open: true,
        }
        .records_peer_send_failure());
        assert!(Error::TransportNotReady {
            state: WebrtcConnectionState::Failed,
            data_channel_open: true,
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
    fn pre_acceptance_backpressure_is_deferrable_and_never_degrades_peer_quality() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let backpressure = [
            Error::DataChannelSendQueueTimeout {
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
    fn post_acceptance_timeouts_are_ambiguous_and_not_retryable() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let ambiguous = [
            Error::DataChannelSendCompletionTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::DataChannelDeliveryTimeout {
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
    fn data_plane_deferral_errors_require_fresh_topology() {
        let peer: crate::dht::Did = crate::ecc::SecretKey::random().address().into();
        let deferrals = [
            Error::DataChannelSendQueueTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::ConnectionAttemptSuperseded {
                peer,
                generation: 1,
            },
            Error::RTCDataChannelStateNotOpen,
            Error::TransportNotReady {
                state: WebrtcConnectionState::Disconnected,
                data_channel_open: true,
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
