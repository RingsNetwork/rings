//! This module contains the [ConnectionRef] struct.

use std::sync::Arc;
use std::sync::Weak;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;
use crate::PlatformSendSync;

/// [ConnectionRef] is a weak reference to a connection and implements the `ConnectionInterface` trait.
/// When the connection is dropped, it returns an error called [Error::ConnectionReleased].
/// It serves as the return value for the `connection` method of [TransportInterface](crate::core::transport::TransportInterface).
pub struct ConnectionRef<C> {
    cid: String,
    conn: Weak<C>,
}

impl<C> Clone for ConnectionRef<C> {
    fn clone(&self) -> Self {
        Self {
            cid: self.cid.clone(),
            conn: self.conn.clone(),
        }
    }
}

impl<C> ConnectionRef<C> {
    /// Create a new connection reference.
    pub fn new(cid: &str, conn: &Arc<C>) -> Self {
        Self {
            cid: cid.to_string(),
            conn: Arc::downgrade(conn),
        }
    }

    pub(crate) fn upgrade(&self) -> Result<Arc<C>> {
        match self.conn.upgrade() {
            Some(conn) => Ok(conn),
            None => Err(Error::ConnectionReleased(self.cid.clone())),
        }
    }

    pub(crate) fn cid(&self) -> &str {
        &self.cid
    }

    pub(crate) fn points_to(&self, connection: &Arc<C>) -> bool {
        self.conn.ptr_eq(&Arc::downgrade(connection))
    }
}

#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
impl ConnectionRef<crate::connections::WebrtcConnection> {
    /// Capture a witness that survives pool removal and observes completed native close.
    pub fn physical_close_witness(&self) -> Result<crate::connections::NativePhysicalCloseWitness> {
        Ok(self.upgrade()?.physical_close_witness())
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl ConnectionRef<crate::connections::DummyConnection> {
    /// Test hook: force a dummy connection state without dispatching lifecycle callbacks.
    pub fn force_dummy_webrtc_connection_state_without_callback(
        &self,
        state: WebrtcConnectionState,
    ) -> Result<()> {
        self.upgrade()?
            .force_webrtc_connection_state_without_callback(state);
        Ok(())
    }

    /// Test hook: override dummy data-channel readiness without dispatching callbacks.
    pub fn force_dummy_data_channel_open_without_callback(&self, open: Option<bool>) -> Result<()> {
        self.upgrade()?
            .force_data_channel_open_without_callback(open);
        Ok(())
    }
}

#[cfg_attr(
    all(feature = "web-sys-webrtc", target_family = "wasm"),
    async_trait(?Send)
)]
#[cfg_attr(
    not(all(feature = "web-sys-webrtc", target_family = "wasm")),
    async_trait
)]
impl<C, S> ConnectionInterface for ConnectionRef<C>
where
    C: ConnectionInterface<Error = Error, Sdp = S> + PlatformSendSync,
    for<'async_trait> S: Serialize + DeserializeOwned + Send + Sync + 'async_trait,
{
    type Sdp = C::Sdp;
    type Error = C::Error;

    async fn send_message(&self, msg: TransportMessage) -> Result<DeliveryFuture> {
        self.upgrade()?.send_message(msg).await
    }

    async fn send_message_with_permit(
        &self,
        msg: TransportMessage,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        self.upgrade()?.send_message_with_permit(msg, permit).await
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.upgrade()
            .map(|c| c.webrtc_connection_state())
            .unwrap_or(WebrtcConnectionState::Closed)
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.upgrade()
            .map(|connection| connection.connection_state_snapshot())
            .unwrap_or(ConnectionStateSnapshot::new(
                WebrtcConnectionState::Closed,
                false,
            ))
    }

    fn data_channel_is_open(&self) -> Result<bool> {
        match self.upgrade() {
            Ok(c) => c.data_channel_is_open(),
            Err(Error::ConnectionReleased(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    // On a released reference this reports the interop default rather than an error, by deliberate
    // design: `ConnectionInterface::max_message_size` returns `usize` (it feeds the framing
    // planner), and threading a `Result` through it and every backend for this one edge would add
    // churn out of proportion to the case. It is harmless because a send on a released ref fails
    // anyway (`send_message` upgrades the same `Weak` and returns `ConnectionReleased`), so the
    // framing plan computed against the default is never actually transmitted. See the
    // `released_ref_*` tests.
    fn max_message_size(&self) -> usize {
        self.upgrade()
            .map(|c| c.max_message_size())
            .unwrap_or(MAX_DATA_CHANNEL_MESSAGE_SIZE)
    }

    async fn get_stats(&self) -> Vec<String> {
        let Ok(c) = self.upgrade() else {
            return Vec::new();
        };
        c.get_stats().await
    }

    async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_create_offer().await
    }

    async fn webrtc_answer_offer(&self, offer: Self::Sdp) -> Result<Self::Sdp> {
        self.upgrade()?.webrtc_answer_offer(offer).await
    }

    async fn webrtc_accept_answer(&self, answer: Self::Sdp) -> Result<()> {
        self.upgrade()?.webrtc_accept_answer(answer).await
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        self.upgrade()?.webrtc_wait_for_data_channel_open().await
    }

    async fn close(&self) -> Result<()> {
        self.upgrade()?.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `ConnectionInterface` whose only meaningful method is `max_message_size` (a sentinel
    /// distinct from the interop default). The released-ref path never dispatches to the others, so
    /// they assert that invariant by panicking if ever reached.
    #[derive(Debug)]
    struct Mock;

    #[cfg_attr(all(feature = "web-sys-webrtc", target_family = "wasm"), async_trait(?Send))]
    #[cfg_attr(
        not(all(feature = "web-sys-webrtc", target_family = "wasm")),
        async_trait
    )]
    impl ConnectionInterface for Mock {
        type Sdp = String;
        type Error = Error;

        fn max_message_size(&self) -> usize {
            4242
        }

        async fn send_message(&self, _: TransportMessage) -> Result<DeliveryFuture> {
            unreachable!("a released ref must fail before reaching the inner connection")
        }
        async fn send_message_with_permit(
            &self,
            _: TransportMessage,
            _: SendPermit,
        ) -> Result<DeliveryFuture> {
            unreachable!("a released ref must fail before reaching the inner connection")
        }
        fn webrtc_connection_state(&self) -> WebrtcConnectionState {
            unreachable!()
        }
        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            unreachable!()
        }
        fn data_channel_is_open(&self) -> Result<bool> {
            unreachable!()
        }
        async fn get_stats(&self) -> Vec<String> {
            unreachable!()
        }
        async fn webrtc_create_offer(&self) -> Result<Self::Sdp> {
            unreachable!()
        }
        async fn webrtc_answer_offer(&self, _: Self::Sdp) -> Result<Self::Sdp> {
            unreachable!()
        }
        async fn webrtc_accept_answer(&self, _: Self::Sdp) -> Result<()> {
            unreachable!()
        }
        async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
            unreachable!()
        }
        async fn close(&self) -> Result<()> {
            unreachable!()
        }
    }

    /// A live ref forwards to the inner connection; a released one reports the interop default
    /// rather than erroring (see the deliberate-design note on `max_message_size`).
    #[test]
    fn test_released_ref_max_message_size_falls_back_to_default() {
        let conn = Arc::new(Mock);
        let reference = ConnectionRef::new("cid", &conn);
        assert_eq!(reference.max_message_size(), 4242, "live ref forwards");

        drop(conn);
        assert_eq!(
            reference.max_message_size(),
            MAX_DATA_CHANNEL_MESSAGE_SIZE,
            "released ref reports the interop default"
        );
    }

    /// A released ref surfaces the release as an error on the data path (so the default reported
    /// above is never actually transmitted against).
    #[test]
    fn test_released_ref_upgrade_errors() {
        let conn = Arc::new(Mock);
        let reference = ConnectionRef::new("cid", &conn);
        assert!(reference.upgrade().is_ok(), "live ref upgrades");

        drop(conn);
        match reference.upgrade() {
            Err(Error::ConnectionReleased(cid)) => assert_eq!(cid, "cid"),
            other => panic!("released ref must report ConnectionReleased, got {other:?}"),
        }
    }
}
