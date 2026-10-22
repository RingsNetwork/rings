//! Outbound scheduler shutdown racing a send at each of its gates: before queue acceptance,
//! at transport dispatch, and after the backend accepted the frame.

use rings_transport::connections::dummy_controlled;

use super::super::super::delivery::SendCompletionOutcome;
use crate::error::Result;
use crate::swarm::transport::tests::test_lifecycle::transport_with_routable_peer;
use crate::swarm::Arc;
use crate::swarm::Error;
use crate::swarm::Message;
use crate::swarm::MessagePayload;
#[tokio::test]
async fn test_scheduler_shutdown_revokes_a_frame_waiting_at_transport_dispatch() -> Result<()> {
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"shutdown-at-dispatch")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::pause_send_message_at_dispatch();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move { sending_transport.send_payload_admitted(payload).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        dummy_controlled::send_message_gate_entered(),
    )
    .await
    .map_err(|_| Error::InvalidMessage("send did not reach dispatch gate".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    dummy_controlled::release_send_message_gate();

    let outcome = send
        .await
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(dummy_controlled::sent_count(), 0);
    Ok(())
}

#[tokio::test]
async fn test_scheduler_shutdown_after_backend_acceptance_preserves_detached_success() -> Result<()>
{
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"shutdown-after-backend-acceptance")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::set_drop_messages(true);
    dummy_controlled::pause_irrevocable_send();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move { sending_transport.send_payload_admitted(payload).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        dummy_controlled::irrevocable_send_gate_entered(),
    )
    .await
    .map_err(|_| Error::InvalidMessage("send did not become irrevocable".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    dummy_controlled::release_irrevocable_send_gate();

    let outcome = send
        .await
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    dummy_controlled::set_drop_messages(false);
    assert_eq!(outcome, SendCompletionOutcome::Succeeded);
    assert_eq!(dummy_controlled::sent_count(), 1);
    assert_eq!(
        transport.outbound_admitted_transfer_total_for_test(),
        0,
        "accepted completion must publish only after scheduler capacity is released"
    );
    Ok(())
}

#[tokio::test]
async fn test_scheduler_shutdown_cancels_a_send_before_queue_acceptance() -> Result<()> {
    let (transport, peer, _attempt) = transport_with_routable_peer().await?;
    let payload = MessagePayload::new_send(
        Message::custom(b"linearized-before-shutdown")?,
        transport.message_signer(),
        peer,
        peer,
    )?;
    dummy_controlled::reset_sent_count();
    dummy_controlled::set_drop_messages(true);
    dummy_controlled::pause_send_message_after_permit();
    let sending_transport = Arc::clone(&transport);
    let send = tokio::spawn(async move { sending_transport.send_payload_admitted(payload).await });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        dummy_controlled::post_permit_send_gate_entered(),
    )
    .await
    .map_err(|_| Error::InvalidMessage("send did not reach its acceptance gate".to_string()))?;
    transport.outbound_schedulers.shutdown(peer);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), send)
        .await
        .map_err(|_| Error::InvalidMessage("unaccepted send did not cancel".to_string()))?
        .map_err(|error| Error::InvalidMessage(format!("send task failed: {error}")))??;
    dummy_controlled::release_post_permit_send_gate();
    dummy_controlled::set_drop_messages(false);
    assert_eq!(outcome, SendCompletionOutcome::Cancelled);
    assert_eq!(dummy_controlled::sent_count(), 0);
    Ok(())
}
