//! Each lane's credit pump: the only sender of credit frames.
//!
//! ```text
//!   LinkCredit::next_credit(lane) ─▶ send_credit ─ok──▶ next
//!                                                └err─▶ pause ─▶ requeue ─▶ next
//! ```

use std::sync::Arc;
use std::time::Duration;

use rings_runtime::MaybeSendSync;

use super::link_credit::LinkCredit;
use super::link_send::FrameSink;
use crate::connection_ref::ConnectionRef;
use crate::core::credit::CreditIndex;
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

/// Send `credit` on `lane` of `connection`, once its data channels are open. A credit frame is
/// never charged against credit, so the send never waits on the peer.
async fn send_credit<C>(
    connection: &ConnectionRef<C>,
    credit: u64,
    lane: ChannelLane,
) -> Result<DeliveryFuture>
where
    C: FrameSink + MaybeSendSync,
{
    let connection = connection.upgrade()?;
    connection.webrtc_wait_for_data_channel_open().await?;
    connection
        .send_frame(TransportMessage::Credit(credit), lane, SendPermit::always())
        .await
}

/// Run the credit pump of the lane of `index`: send every credit `link` queues on the lane as a
/// [`TransportMessage::Credit`] frame, until the connection is gone.
///
/// A credit is cumulative, so losing one can leave the peer's sender waiting for good: a credit
/// whose send fails is queued again and sent after `pause`, [`CREDIT_RESEND_PAUSE`] in a
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
    while let Some(credit) = link.next_credit(index).await {
        let Err(error) = send_credit(&connection, credit, index.lane()).await else {
            continue;
        };
        // The connection is gone for good: no send of it can succeed again.
        if matches!(error, Error::ConnectionReleased(_)) {
            return;
        }
        tracing::debug!(
            peer = %link.peer,
            error = ?error,
            "failed to send a credit frame; resending"
        );
        if rings_runtime::sleep(pause).await.is_err() || !link.requeue(index, credit) {
            return;
        }
    }
}
