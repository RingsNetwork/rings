//! The detached caller boundary when the outbound worker is lost mid-send (#860 review,
//! lemma (P)): what `do_send_payload_detached_until` returns depends only on whether the
//! backend had claimed the first frame.

use std::time::Duration;

use futures::future::select;
use futures::future::Either;
use futures::pin_mut;
use rings_transport::connections::dummy_controlled;

use super::transport_with_routable_peer;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::swarm::transport::SendCompletionOutcome;

/// Where the dummy backend parks the send when the worker is lost.
#[derive(Clone, Copy, Debug)]
enum Gate {
    /// After the permit check, before the claim: nothing reached the backend.
    PostPermit,
    /// After the claim: the backend owns the frame.
    Irrevocable,
}

impl Gate {
    /// Park this thread's next dummy send at the gate.
    fn pause(self) {
        match self {
            Self::PostPermit => dummy_controlled::pause_send_message_after_permit(),
            Self::Irrevocable => dummy_controlled::pause_irrevocable_send(),
        }
    }

    /// Release a send parked at the gate.
    fn release(self) {
        match self {
            Self::PostPermit => dummy_controlled::release_post_permit_send_gate(),
            Self::Irrevocable => dummy_controlled::release_irrevocable_send_gate(),
        }
    }
}

/// Send detached until the backend parks the frame at `gate`, lose the worker there (its
/// runtime shuts down, dropping it), and return what the caller observes.
///
/// The send races the gate's entry notification, so a send that never parks fails the test
/// by completing instead of spinning.
fn send_losing_the_worker_at(gate: Gate) -> Result<Result<SendCompletionOutcome>> {
    let worker_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::InvalidMessage(format!("runtime: {error}")))?;
    let (transport, peer, _attempt) = worker_runtime.block_on(transport_with_routable_peer())?;
    let payload = MessagePayload::new_send(
        Message::custom(b"worker-lost-mid-send")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::set_drop_messages(true);
    gate.pause();
    let entered = dummy_controlled::send_gate_entered();
    let send = transport.send_payload_detached_until_for_test(
        payload,
        Duration::from_secs(60),
        futures::future::pending(),
    );
    pin_mut!(send);
    let parked = worker_runtime.block_on(async {
        let entry = entered.notified();
        pin_mut!(entry);
        matches!(select(send.as_mut(), entry).await, Either::Right(_))
    });
    assert!(parked, "the send parks at {gate:?} before completing");
    // Runtime cancellation drops the worker mid-send: it publishes `Cancelled`.
    drop(worker_runtime);
    let outcome = futures::executor::block_on(send);
    gate.release();
    dummy_controlled::set_drop_messages(false);
    Ok(outcome)
}

/// Regression (lemma (P)): a worker lost after the claim yields the ambiguous
/// `DetachedSendAbandonedAfterClaim`; one lost before the claim yields `Cancelled`, a
/// pre-acceptance deferral. Without the gate at the boundary the claimed case returns
/// `Ok(Cancelled)`; with a gate that always answered ambiguously the unclaimed case would not
/// return `Cancelled`.
#[test]
fn test_worker_lost_mid_send_is_ambiguous_exactly_after_the_claim() -> Result<()> {
    let unclaimed = send_losing_the_worker_at(Gate::PostPermit)?;
    assert!(
        matches!(unclaimed, Ok(SendCompletionOutcome::Cancelled)),
        "{unclaimed:?}"
    );
    let claimed = send_losing_the_worker_at(Gate::Irrevocable)?;
    assert!(
        matches!(claimed, Err(Error::DetachedSendAbandonedAfterClaim { .. })),
        "{claimed:?}"
    );
    Ok(())
}
