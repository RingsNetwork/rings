//! Weak connection reference handle [`ConnectionRef`] implementing [`ConnectionInterface`].

use std::any::Any;
use std::future::ready;
use std::sync::Arc;
use std::sync::Weak;

use async_trait::async_trait;
use bytes::Bytes;
use rings_runtime::MaybeSendSync;

use crate::core::pool::ChannelLane;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::DynConnection;
use crate::core::transport::HandshakeStep;
use crate::core::transport::LaneCreditView;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportConnectionState;
use crate::core::transport::MAX_CHANNEL_MESSAGE_SIZE;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::error::Result;

/// Weak reference to a pooled connection generation that forwards [`ConnectionInterface`] calls
/// while the physical connection remains live in [`crate::connection_pool::Pool`].
///
/// Once the connection generation is retired from the pool and dropped, operations return
/// [`Error::ConnectionReleased`].
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
    /// Create a new weak [`ConnectionRef`] bound to `cid` and `conn`.
    pub fn new(cid: &str, conn: &Arc<C>) -> Self {
        Self {
            cid: cid.to_string(),
            conn: Arc::downgrade(conn),
        }
    }

    /// Upgrade this weak handle to a temporary strong [`Arc`] to the live connection, returning
    /// [`Error::ConnectionReleased`] if the physical connection generation has already been
    /// retired and dropped.
    ///
    /// # Lifetime Contract
    ///
    /// Callers MUST drop the returned [`Arc`] promptly at the end of the single operation being
    /// performed and MUST NOT store it across asynchronous waits or in long-lived structures;
    /// holding a strong [`Arc`] pins the underlying connection and its callback state past pool
    /// retirement.
    pub fn upgrade(&self) -> Result<Arc<C>> {
        match self.conn.upgrade() {
            Some(conn) => Ok(conn),
            None => Err(Error::ConnectionReleased(self.cid.clone())),
        }
    }

    /// Return the connection identifier (`cid`) bound to this handle.
    pub fn cid(&self) -> &str {
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
where C: ConnectionInterface<Error = Error> + MaybeSendSync + 'static
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

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<C> ConnectionInterface for ConnectionRef<C>
where C: ConnectionInterface<Error = Error> + MaybeSendSync + ?Sized + 'static
{
    type Error = C::Error;

    fn as_any(&self) -> &dyn Any {
        self
    }

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

    fn max_message_size(&self) -> usize {
        self.upgrade()
            .map(|c| c.max_message_size())
            .unwrap_or(MAX_CHANNEL_MESSAGE_SIZE)
    }

    async fn handshake_step(&self, inbound: Option<Bytes>) -> Result<HandshakeStep> {
        self.upgrade()?.handshake_step(inbound).await
    }

    async fn wait_for_channel_open(&self) -> Result<()> {
        self.upgrade()?.wait_for_channel_open().await
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

    #[cfg_attr(target_family = "wasm", async_trait(?Send))]
    #[cfg_attr(not(target_family = "wasm"), async_trait)]
    impl ConnectionInterface for Mock {
        type Error = Error;

        fn as_any(&self) -> &dyn Any {
            self
        }

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
        async fn handshake_step(&self, _: Option<Bytes>) -> Result<HandshakeStep> {
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
    /// rather than erroring.
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

    /// A released ref surfaces the release as an error on the data path.
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
