//! This module contains the [ConnectionRef] struct.

use std::future::ready;
use std::sync::Arc;
use std::sync::Weak;

use async_trait::async_trait;
use bytes::Bytes;
use rings_runtime::MaybeSendSync;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::core::pool::ChannelLane;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::DynConnection;
use crate::core::transport::LaneCreditView;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::error::Result;

/// [ConnectionRef] is a weak reference to a connection and implements the `ConnectionInterface` trait.
/// When the connection is dropped, it returns an error called [Error::ConnectionReleased].
/// It serves as the return value for the `connection` method of [TransportInterface](crate::core::transport::TransportInterface).
pub struct ConnectionRef<C: ?Sized = DynConnection> {
    cid: String,
    conn: Weak<C>,
}

impl<C: ?Sized> Clone for ConnectionRef<C> {
    fn clone(&self) -> Self {
        Self {
            cid: self.cid.clone(),
            conn: self.conn.clone(),
        }
    }
}

impl<C: ?Sized> ConnectionRef<C> {
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

    /// Return whether this weak reference and `connection` point to the exact same physical
    /// allocation, comparing thin data pointers (`*const ()`) so fat-pointer vtable addresses do
    /// not affect equality across trait-object coercion or codegen units.
    pub(crate) fn points_to<T: ?Sized>(&self, connection: &Arc<T>) -> bool {
        std::ptr::eq(
            self.conn.as_ptr().cast::<()>(),
            Arc::as_ptr(connection).cast::<()>(),
        )
    }

    /// Return whether two weak connection references (possibly of different type erasures) refer
    /// to the exact same physical allocation.
    pub fn same_allocation<T: ?Sized>(&self, other: &ConnectionRef<T>) -> bool {
        std::ptr::eq(
            self.conn.as_ptr().cast::<()>(),
            other.conn.as_ptr().cast::<()>(),
        )
    }
}

impl<C> ConnectionRef<C>
where C: ConnectionInterface<Handshake = String, Error = Error> + MaybeSendSync + 'static
{
    /// Erase the concrete connection type parameter into [`ConnectionRef<DynConnection>`],
    /// preserving the underlying weak allocation pointer for slot-identity checks.
    pub fn into_dyn(self) -> ConnectionRef<DynConnection> {
        let conn: Weak<DynConnection> = self.conn;
        ConnectionRef {
            cid: self.cid,
            conn,
        }
    }
}

#[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
impl<C> ConnectionRef<C>
where C: ConnectionInterface<Error = Error> + ?Sized
{
    /// Capture a witness that survives pool removal and observes completed native close.
    pub fn physical_close_witness(&self) -> Result<crate::connections::NativePhysicalCloseWitness> {
        self.upgrade()?.physical_close_witness()
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl<C> ConnectionRef<C>
where C: ConnectionInterface<Error = Error> + ?Sized
{
    /// Stable physical generation identifier used by deterministic dummy simulations.
    pub fn dummy_generation_id(&self) -> Result<String> {
        self.upgrade()?.dummy_generation_id()
    }

    /// Test hook: force a dummy connection state without dispatching lifecycle callbacks.
    pub fn force_dummy_connection_state_without_callback(
        &self,
        state: TransportConnectionState,
    ) -> Result<()> {
        self.upgrade()?
            .force_dummy_connection_state_without_callback(state)
    }

    /// Test hook: override dummy data-channel readiness without dispatching callbacks.
    pub fn force_dummy_channel_open_without_callback(&self, open: Option<bool>) -> Result<()> {
        self.upgrade()?
            .force_dummy_channel_open_without_callback(open)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<C, S> ConnectionInterface for ConnectionRef<C>
where
    C: ConnectionInterface<Error = Error, Handshake = S> + MaybeSendSync + ?Sized,
    for<'async_trait> S: Serialize + DeserializeOwned + Send + Sync + 'async_trait,
{
    type Handshake = C::Handshake;
    type Error = C::Error;

    async fn send_message_with_permit(
        &self,
        payload: Bytes,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        self.upgrade()?
            .send_message_with_permit(payload, lane, permit)
            .await
    }

    /// The wait is taken from the connection and the connection let go at once: a long wait
    /// keeps the connection alive no longer than this call.
    fn reserve_send_credit(&self, lane: ChannelLane) -> SendCreditWait<Error> {
        match self.upgrade() {
            Ok(connection) => connection.reserve_send_credit(lane),
            Err(error) => Box::pin(ready(Err(error))),
        }
    }

    /// The view is taken from the connection, which is let go at once; a released connection
    /// has no credit to view.
    fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
        self.upgrade().map_or_else(
            |_| LaneCreditView::closed(),
            |connection| connection.send_credit_view(lane),
        )
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        self.upgrade()
            .map(|connection| connection.connection_state_snapshot())
            .unwrap_or(ConnectionStateSnapshot::new(
                TransportConnectionState::Closed,
                false,
            ))
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
            .unwrap_or(MAX_CHANNEL_MESSAGE_SIZE)
    }

    async fn create_offer(&self) -> Result<Self::Handshake> {
        self.upgrade()?.create_offer().await
    }

    async fn answer_offer(&self, offer: Self::Handshake) -> Result<Self::Handshake> {
        self.upgrade()?.answer_offer(offer).await
    }

    async fn accept_answer(&self, answer: Self::Handshake) -> Result<()> {
        self.upgrade()?.accept_answer(answer).await
    }

    async fn wait_for_channel_open(&self) -> Result<()> {
        self.upgrade()?.wait_for_channel_open().await
    }

    async fn close(&self) -> Result<()> {
        self.upgrade()?.close().await
    }

    #[cfg(all(feature = "native-webrtc", not(target_family = "wasm")))]
    fn physical_close_witness(&self) -> Result<crate::connections::NativePhysicalCloseWitness> {
        self.upgrade()?.physical_close_witness()
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn dummy_generation_id(&self) -> Result<String> {
        self.upgrade()?.dummy_generation_id()
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn force_dummy_connection_state_without_callback(
        &self,
        state: TransportConnectionState,
    ) -> Result<()> {
        self.upgrade()?
            .force_dummy_connection_state_without_callback(state)
    }

    #[cfg(all(feature = "dummy", not(target_family = "wasm")))]
    fn force_dummy_channel_open_without_callback(&self, open: Option<bool>) -> Result<()> {
        self.upgrade()?
            .force_dummy_channel_open_without_callback(open)
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

    #[cfg_attr(target_family = "wasm", async_trait(?Send))]
    #[cfg_attr(not(target_family = "wasm"), async_trait)]
    impl ConnectionInterface for Mock {
        type Handshake = String;
        type Error = Error;

        fn max_message_size(&self) -> usize {
            4242
        }

        async fn send_message_with_permit(
            &self,
            _: Bytes,
            _: ChannelLane,
            _: SendPermit,
        ) -> Result<DeliveryFuture> {
            unreachable!("a released ref must fail before reaching the inner connection")
        }
        fn reserve_send_credit(&self, _: ChannelLane) -> crate::delivery::SendCreditWait<Error> {
            unreachable!("a released ref must fail before reaching the inner connection")
        }
        fn send_credit_view(&self, _: ChannelLane) -> LaneCreditView {
            unreachable!("a released ref must fail before reaching the inner connection")
        }
        fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
            unreachable!()
        }
        async fn create_offer(&self) -> Result<Self::Handshake> {
            unreachable!()
        }
        async fn answer_offer(&self, _: Self::Handshake) -> Result<Self::Handshake> {
            unreachable!()
        }
        async fn accept_answer(&self, _: Self::Handshake) -> Result<()> {
            unreachable!()
        }
        async fn wait_for_channel_open(&self) -> Result<()> {
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
            MAX_CHANNEL_MESSAGE_SIZE,
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
