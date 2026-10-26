//! Each lane's pump, the only sender of its credit signals, and each link's idle-check driver.
//!
//! ```text
//!   LinkCredit::next_signal(lane) ─▶ send_signal ─ok──▶ next
//!                                                └err─▶ pause ─▶ requeue ─▶ next
//!   every LANE_IDLE_PERIOD ─▶ LinkCredit::idle_check ─false─▶ end
//! ```

use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use rings_runtime::MaybeSendSync;

use super::link_credit::LinkCredit;
use super::link_send::FrameSink;
use crate::connection_ref::ConnectionRef;
use crate::core::credit::CreditIndex;
use crate::core::credit::LaneSignal;
use crate::core::pool::ChannelLane;
use crate::core::transport::SendPermit;
use crate::core::transport::TransportMessage;
use crate::delivery::DeliveryFuture;
use crate::error::Error;
use crate::error::Result;

/// How long a lane's credit pump pauses after a failed send before it sends the credit again:
/// a send fails transiently when one of the connection's outbound channels is closed or not yet
/// open, and the pause keeps a send that fails at once from spinning the pump.
pub(crate) const CREDIT_RESEND_PAUSE: Duration = Duration::from_secs(1);

/// Send `signal` on `lane` of `connection`, once its data channels are open. A signal frame is
/// never charged against credit, so the send never waits on the peer.
async fn send_signal<C>(
    connection: &ConnectionRef<C>,
    signal: LaneSignal,
    lane: ChannelLane,
) -> Result<DeliveryFuture>
where
    C: FrameSink + MaybeSendSync,
{
    let connection = connection.upgrade()?;
    connection.webrtc_wait_for_data_channel_open().await?;
    connection
        .send_frame(TransportMessage::Signal(signal), lane, SendPermit::always())
        .await
}

/// Run the pump of the lane of `index`: send every signal `link` queues on the lane as a
/// [`TransportMessage::Signal`] frame, until the connection is gone.
///
/// A credit is cumulative, so losing one can leave the peer's sender waiting for good, and a
/// lost shrink or return leaves the lane's pooled share unreturned: a signal whose send fails is
/// queued again and sent after `pause`, [`CREDIT_RESEND_PAUSE`] in a
/// backend (a send fails transiently while any of this end's outbound channels is closed or not
/// yet open; credits leave on the outbound channel, frames arrive on the remote-created one),
/// until the connection is gone, which a send failing with the connection released also says.
/// One pump runs per lane, so a credit held up behind one lane's channel delays no other
/// lane's.
pub(crate) async fn pump_lane_credits<C>(
    link: Arc<LinkCredit>,
    connection: ConnectionRef<C>,
    index: CreditIndex,
    pause: Duration,
) where
    C: FrameSink + MaybeSendSync,
{
    while let Some(signal) = link.next_signal(index).await {
        let Err(error) = send_signal(&connection, signal, index.lane()).await else {
            continue;
        };
        // The connection is gone for good: no send of it can succeed again.
        if matches!(error, Error::ConnectionReleased(_)) {
            return;
        }
        tracing::debug!(
            peer = %link.peer,
            error = ?error,
            "failed to send a credit signal; resending"
        );
        if rings_runtime::sleep(pause).await.is_err() || !link.requeue(index, signal) {
            return;
        }
    }
}

/// Run `link`'s idle check every `period` until the connection is gone. The driver holds the
/// link weakly, so it keeps no connection alive.
pub(crate) async fn check_idle_lanes(link: Weak<LinkCredit>, period: Duration) {
    loop {
        if rings_runtime::sleep(period).await.is_err() {
            return;
        }
        if !link.upgrade().is_some_and(|link| link.idle_check()) {
            return;
        }
    }
}
