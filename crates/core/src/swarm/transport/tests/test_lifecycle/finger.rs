use super::*;

#[tokio::test]
async fn test_finger_update_serializes_with_generation_retirement() -> Result<()> {
    let (transport, peer, attempt) = transport_with_routable_peer().await?;
    let request = finger_request(&transport, 0)?;
    let (hold_lifecycle, lifecycle_gate) = lifecycle_test_gate();
    let finger_transport = Arc::clone(&transport);
    let finger_thread = BoundedThread::spawn(move || {
        finger_transport.record_finger_candidate_with_observer_for_test(
            peer,
            request,
            hold_lifecycle,
        )
    });
    lifecycle_gate.wait_until_entered()?;
    let retirement = BlockedRetirement::spawn(Arc::clone(&transport), peer, attempt);
    retirement.wait_until_registered(&transport)?;

    lifecycle_gate.release()?;
    assert_eq!(
        finger_thread.finish("finger update")??,
        FingerUpdateDisposition::Applied
    );
    assert_eq!(retirement.finish()?, Some(()));
    assert!(!transport.is_admitted_connection(peer));
    assert!(!transport.dht.lock_finger()?.contains(Some(peer)));
    Ok(())
}
