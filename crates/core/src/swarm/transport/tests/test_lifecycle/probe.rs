//! A probe pass runs alone, and a probe rides only its own generation's connection.

use super::super::super::egress::QueueAdmission;
use super::super::super::outbound::SenderBound;
use crate::error::Result;
use crate::swarm::transport::tests::test_lifecycle::transport_with_routable_peer;
use crate::swarm::Error;
use crate::swarm::Message;
use crate::swarm::MessagePayload;
use crate::utils::get_epoch_ms_i64;

/// Single flight: while one probe pass is open, another finds none to run, and the right
/// returns when the open pass ends.
#[tokio::test]
async fn test_one_probe_pass_runs_at_a_time() -> Result<()> {
    let (transport, _peer, _attempt) = transport_with_routable_peer().await?;
    let now_ms = get_epoch_ms_i64();
    let open = transport.liveness_probe_pass(now_ms)?;
    assert!(open.is_some(), "no pass is running, so this one opens");
    assert!(
        transport.liveness_probe_pass(now_ms)?.is_none(),
        "a second pass does not run beside the open one"
    );
    drop(open);
    assert!(
        transport.liveness_probe_pass(now_ms)?.is_some(),
        "the right returns when the open pass ends"
    );
    Ok(())
}

/// Pinned: a probe of a generation other than the active one is refused before the queue, so
/// it never rides a successor's connection.
#[tokio::test]
async fn test_a_probe_rides_only_its_own_generation() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"probe-of-a-retired-generation")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    let stale = attempt.generation.wrapping_sub(1);
    let refused = transport
        .send_payload_enqueued(
            peer,
            payload,
            QueueAdmission::Bounded(SenderBound::Probe { generation: stale }),
            None,
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(Error::ConnectionAttemptSuperseded { peer: refused_peer, generation })
                if refused_peer == peer && generation == stale
        ),
        "a probe of another generation is refused: {refused:?}"
    );
    Ok(())
}
