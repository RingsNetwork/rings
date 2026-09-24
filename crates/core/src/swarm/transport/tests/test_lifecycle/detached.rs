//! The detached caller boundary when the outbound worker is lost after its first frame was
//! claimed (#860 review, lemma (P)).

use std::time::Duration;

use futures::pin_mut;
use futures::poll;
use rings_transport::connections::dummy_controlled;

use super::transport_with_routable_peer;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;

/// Regression (lemma (P)): the runtime that owns the outbound worker shuts down while the
/// backend holds a claimed first frame. The dropped worker publishes `Cancelled`, and
/// `do_send_payload_detached_until` must return the ambiguous `DetachedSendAbandonedAfterClaim`
/// instead of a retriable `Cancelled`. Without the gate at that boundary the send returns
/// `Ok(Cancelled)` and this test fails.
#[test]
fn test_worker_lost_after_a_claim_is_ambiguous_at_the_detached_boundary() -> Result<()> {
    let worker_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::InvalidMessage(format!("runtime: {error}")))?;
    let (transport, peer, _attempt) = worker_runtime.block_on(transport_with_routable_peer())?;
    let payload = MessagePayload::new_send(
        Message::custom(b"worker-lost-after-claim")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::set_drop_messages(true);
    dummy_controlled::pause_irrevocable_send();
    let send = transport.send_payload_detached_until_for_test(
        payload,
        Duration::from_secs(60),
        futures::future::pending(),
    );
    pin_mut!(send);
    // Drive the caller and the worker on the worker's runtime until the frame is claimed.
    let claimed = worker_runtime.block_on(async {
        while !dummy_controlled::irrevocable_send_gate_waiting() {
            if poll!(send.as_mut()).is_ready() {
                return false;
            }
            tokio::task::yield_now().await;
        }
        true
    });
    assert!(
        claimed,
        "the first frame reaches the backend's irrevocable gate"
    );
    // Runtime cancellation drops the worker mid-send: it publishes `Cancelled`.
    drop(worker_runtime);
    let outcome = futures::executor::block_on(send);
    dummy_controlled::release_irrevocable_send_gate();
    dummy_controlled::set_drop_messages(false);
    assert!(
        matches!(
            outcome,
            Err(Error::DetachedSendAbandonedAfterClaim { peer: abandoned }) if abandoned == peer
        ),
        "{outcome:?}"
    );
    Ok(())
}
