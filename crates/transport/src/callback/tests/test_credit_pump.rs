//! The credit pump against a connection whose credit sends fail.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use super::admit_window;
use super::callback;
use super::credit_pump::pump_lane_credits;
use super::link_send::FrameSink;
use super::wire;
use crate::connection_ref::ConnectionRef;
use crate::core::credit::credit_index;
use crate::core::credit::LaneSignal;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::LaneCreditView;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::error::Result;

/// A connection whose first `failures` credit sends fail as a closed outbound channel does,
/// and which records every credit it sends after that.
struct FlakyCreditConnection {
    /// Credit sends still to fail.
    failures: AtomicUsize,
    /// The credits sent.
    sent: Mutex<Vec<u64>>,
    /// Notified on every credit sent.
    notify: Notify,
}

#[async_trait]
impl FrameSink for FlakyCreditConnection {
    async fn send_frame(
        &self,
        frame: TransportMessage,
        _: ChannelLane,
        _: SendPermit,
    ) -> Result<DeliveryFuture> {
        let TransportMessage::Signal(LaneSignal::Credit { limit: credit, .. }) = frame else {
            return Err(Error::DataChannelOpen("only credit frames are sent".into()));
        };
        let failed = self
            .failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failed {
            return Err(Error::DataChannelOpen(
                "an outbound channel is closed".into(),
            ));
        }
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(credit);
        self.notify.notify_waiters();
        Ok(Box::pin(std::future::ready(Ok(()))))
    }
}

/// The pump reaches the connection through [`FrameSink`] alone; the custom send path is
/// unreachable here.
#[async_trait]
impl ConnectionInterface for FlakyCreditConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message_with_permit(
        &self,
        _: Bytes,
        _: ChannelLane,
        _: SendPermit,
    ) -> Result<DeliveryFuture> {
        Err(Error::DataChannelOpen("only credit frames are sent".into()))
    }

    fn reserve_send_credit(&self, _: ChannelLane) -> SendCreditWait<Error> {
        Box::pin(std::future::ready(Err(Error::DataChannelOpen(
            "the test connection sends credit frames only".into(),
        ))))
    }

    fn send_credit_view(&self, _: ChannelLane) -> LaneCreditView {
        LaneCreditView::closed()
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        ConnectionStateSnapshot::new(WebrtcConnectionState::Connected, true)
    }

    fn max_message_size(&self) -> usize {
        crate::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE
    }

    async fn webrtc_create_offer(&self) -> Result<String> {
        Err(Error::DataChannelOpen("no signalling in this test".into()))
    }

    async fn webrtc_answer_offer(&self, _: String) -> Result<String> {
        Err(Error::DataChannelOpen("no signalling in this test".into()))
    }

    async fn webrtc_accept_answer(&self, _: String) -> Result<()> {
        Err(Error::DataChannelOpen("no signalling in this test".into()))
    }

    async fn webrtc_wait_for_data_channel_open(&self) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// A credit whose send fails transiently is sent again by the lane's pump after its pause, so
/// a lost last raise never leaves the peer's sender waiting for good. The pause is injected as
/// zero and the wait is woken by the send; the timeout is a hang guard, not a pace.
#[tokio::test]
async fn test_the_pump_resends_a_credit_whose_send_failed() {
    let callback = callback("peer");
    let lane = ChannelLane::new(2);
    let custom = wire(&TransportMessage::Custom(Bytes::from_static(b"data")));
    drop(admit_window(&callback, &custom, lane));
    let connection = Arc::new(FlakyCreditConnection {
        failures: AtomicUsize::new(1),
        sent: Mutex::new(Vec::new()),
        notify: Notify::new(),
    });
    let pump = tokio::spawn(pump_lane_credits(
        Arc::clone(callback.link_credit()),
        ConnectionRef::new("peer", &connection),
        credit_index(lane),
        Duration::ZERO,
    ));

    let resent = async {
        loop {
            let mut sent = std::pin::pin!(connection.notify.notified());
            sent.as_mut().enable();
            if !connection
                .sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                return;
            }
            sent.await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), resent)
        .await
        .expect("the failed credit is sent again");
    assert_eq!(connection.failures.load(Ordering::Acquire), 0);
    assert_eq!(
        *connection
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![2 * LANE_CREDIT_WINDOW]
    );
    callback.link_credit().close();
    pump.await.expect("the pump stops once the link is closed");
}
