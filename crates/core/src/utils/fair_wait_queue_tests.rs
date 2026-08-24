use super::*;

#[test]
fn wake_round_identity_is_shared_by_clone_and_not_by_sequence() {
    let round = FairWakeRound::new(7);
    let same_round = round.clone();
    let reused_sequence = FairWakeRound::new(7);

    assert_eq!(round, same_round);
    assert_ne!(round, reused_sequence);
}

#[test]
fn repeated_arm_preserves_the_first_handoff_round() {
    let queue = Arc::new(CoordinatedFairWaitQueue::coordinated());
    let FairAdmission::Waiting(_waiter) = queue
        .admit_or_wait(FairCapacityDemand::new(0, 1), (), || None::<()>, |_| {})
        .expect("an unbudgeted blocked request must enqueue")
    else {
        panic!("a blocked request must return a waiter");
    };
    let first = FairWakeRound::new(1);
    let second = FairWakeRound::new(2);

    assert_eq!(
        queue.wake_front_with_handoff(first.clone()),
        FairWakeArm::Armed
    );
    assert_eq!(
        queue.wake_front_with_handoff(second),
        FairWakeArm::AlreadyArmed
    );

    let state = queue
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let wake = state
        .queue
        .front()
        .and_then(|waiter| waiter.wake.as_ref())
        .expect("the queue head must remain armed");
    assert!(matches!(wake, FairWake::Handoff(round) if round == &first));
}
