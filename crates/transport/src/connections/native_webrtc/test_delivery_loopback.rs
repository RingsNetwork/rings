//! Event-driven delivery confirmation over a real native loopback link (#887).
//!
//! Every wait here is on a transport event (channel open, `bufferedamountlow`,
//! `close`); the only timer is the hang guard bounding each test.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;

use super::WebrtcConnection;
use super::WebrtcTransport;
use crate::connection_ref::ConnectionRef;
use crate::core::callback::TransportCallback;
use crate::core::transport::ConnectionInterface;
use crate::core::transport::TransportInterface;
use crate::core::transport::TransportMessage;
use crate::error::Result;

/// Hang guard: a bound on a test that waits only on events, never a pacing.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// Payload size of one test message, as in the #887 loopback measurement.
const CELL_BYTES: usize = 16 * 1024;

/// A callback that ignores every connection event.
struct NoopCallback;

#[async_trait]
impl TransportCallback for NoopCallback {}

/// Two native transports joined over host-only loopback ICE, both kept alive.
struct Loopback {
    /// The offering side; its connection sends.
    _offerer: WebrtcTransport,
    /// The answering side.
    _answerer: WebrtcTransport,
    /// The offerer's connection to the answerer.
    sender: ConnectionRef<WebrtcConnection>,
    /// The answerer's connection to the offerer.
    _receiver: ConnectionRef<WebrtcConnection>,
}

/// Connect two transports and wait until the sender's data channels open.
async fn connect_loopback() -> Result<Loopback> {
    let offerer = WebrtcTransport::new("", None, None);
    let answerer = WebrtcTransport::new("", None, None);
    let sender = offerer
        .new_connection("answerer", Box::new(NoopCallback))
        .await?;
    let receiver = answerer
        .new_connection("offerer", Box::new(NoopCallback))
        .await?;
    let offer = sender.webrtc_create_offer().await?;
    let answer = receiver.webrtc_answer_offer(offer).await?;
    sender.webrtc_accept_answer(answer).await?;
    sender.webrtc_wait_for_data_channel_open().await?;
    Ok(Loopback {
        _offerer: offerer,
        _answerer: answerer,
        sender,
        _receiver: receiver,
    })
}

/// One test cell tagged with its index.
fn cell(index: usize) -> TransportMessage {
    let mut payload = vec![0u8; CELL_BYTES];
    payload[..8].copy_from_slice(&(index as u64).to_be_bytes());
    TransportMessage::Custom(Bytes::from(payload))
}

/// Awaited sends back to back: each delivery future resolves `Ok` on the
/// channel's own event, so a paced sender is never held by a timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_awaited_sends_resolve_on_buffer_events() -> Result<()> {
    tokio::time::timeout(HANG_GUARD, async {
        let link = connect_loopback().await?;
        for index in 0..256 {
            link.sender.send_message(cell(index)).await?.await?;
        }
        link.sender.close().await
    })
    .await
    .expect("awaited sends must resolve on buffer events")
}

/// Many sends pending on the same pool at once: every future resolves `Ok`,
/// however the round-robin pool spreads them over its channels.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_pending_sends_all_resolve() -> Result<()> {
    tokio::time::timeout(HANG_GUARD, async {
        let link = connect_loopback().await?;
        let mut deliveries = Vec::new();
        for index in 0..256 {
            deliveries.push(link.sender.send_message(cell(index)).await?);
        }
        for delivery in join_all(deliveries).await {
            delivery?;
        }
        link.sender.close().await
    })
    .await
    .expect("concurrent deliveries must resolve on buffer events")
}

/// A close with sends still pending resolves every future (`Ok` if its bytes
/// had already flushed, `Err` otherwise); none is left waiting for an event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_close_resolves_every_pending_delivery() -> Result<()> {
    tokio::time::timeout(HANG_GUARD, async {
        let link = connect_loopback().await?;
        let mut deliveries = Vec::new();
        for index in 0..64 {
            deliveries.push(link.sender.send_message(cell(index)).await?);
        }
        link.sender.close().await?;
        join_all(deliveries).await;
        Ok(())
    })
    .await
    .expect("a close must resolve every pending delivery")
}
