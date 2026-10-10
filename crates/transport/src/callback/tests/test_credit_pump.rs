//! The credit pump against a connection whose credit sends fail.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use super::link_credit::pump_lane_credits;
use super::InboundFrameAdmission;
use super::InnerTransportCallback;
use super::NodeReceiveLoad;
use crate::connection_ref::ConnectionRef;
use crate::core::credit::credit_index;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::ConnectionStateSnapshot;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::core::transport::WebrtcConnectionState;
use crate::delivery::DeliveryFuture;
use crate::delivery::SendCreditWait;
use crate::error::Error;
use crate::error::Result;

/// A transport callback that ignores every event: the test never dispatches.
struct IgnoredCallback;

#[async_trait]
impl crate::core::callback::TransportCallback for IgnoredCallback {}

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
impl ConnectionInterface for FlakyCreditConnection {
    type Sdp = String;
    type Error = Error;

    async fn send_message_with_permit(
        &self,
        message: TransportMessage,
        _: ChannelLane,
        _: SendPermit,
    ) -> Result<DeliveryFuture> {
        let TransportMessage::Credit(credit) = message else {
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

    fn reserve_send_credit(&self, _: ChannelLane) -> SendCreditWait<Error> {
        Box::pin(std::future::ready(Err(Error::DataChannelOpen(
            "the test connection sends credit frames only".into(),
        ))))
    }

    fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        WebrtcConnectionState::Connected
    }

    fn connection_state_snapshot(&self) -> ConnectionStateSnapshot {
        ConnectionStateSnapshot::new(WebrtcConnectionState::Connected, true)
    }

    fn data_channel_is_open(&self) -> Result<bool> {
        Ok(true)
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
/// a lost last raise never leaves the peer's sender waiting for good. The wait is woken by the
/// send; the timeout is a hang guard, not a pace.
#[tokio::test]
async fn test_the_pump_resends_a_credit_whose_send_failed() {
    let callback = InnerTransportCallback::new(
        "peer",
        Box::new(IgnoredCallback),
        crate::notifier::Notifier::default(),
        NodeReceiveLoad::new(),
    );
    let lane = ChannelLane::new(2);
    let custom = Bytes::from(
        rings_codec::serialize(&TransportMessage::Custom(Bytes::from_static(b"data")))
            .expect("a custom frame serializes"),
    );
    let held = (0..LANE_CREDIT_WINDOW)
        .map(
            |_| match callback.admit_inbound_frame(custom.clone(), lane) {
                InboundFrameAdmission::Admitted(frame) => frame,
                _ => panic!("a frame within the window is admitted"),
            },
        )
        .collect::<Vec<_>>();
    drop(held);
    let connection = Arc::new(FlakyCreditConnection {
        failures: AtomicUsize::new(1),
        sent: Mutex::new(Vec::new()),
        notify: Notify::new(),
    });
    let pump = tokio::spawn(pump_lane_credits(
        Arc::clone(callback.link_credit()),
        ConnectionRef::new("peer", &connection),
        credit_index(lane),
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
