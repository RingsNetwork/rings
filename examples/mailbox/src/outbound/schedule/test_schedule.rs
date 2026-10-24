use super::*;

/// A message from `a@x` to `recipients`, queued at 0.
fn pending(recipients: &[&str]) -> Pending {
    Pending::new(
        "a@x".to_owned(),
        recipients.iter().map(|r| (*r).to_owned()).collect(),
        0,
    )
}

#[test]
fn test_delivered_recipients_drop_and_a_fully_delivered_message_leaves() {
    let settled = pending(&["b@y"]).settle(vec![("b@y".to_owned(), Outcome::Delivered)], 10);
    assert_eq!(settled, Settled {
        pending: None,
        bounced: vec![],
    });
}

#[test]
fn test_deferred_recipients_retry_on_a_growing_schedule() {
    let mut message = pending(&["b@y"]);
    let mut now = 0;
    let mut delays = Vec::new();
    for _ in 0..7 {
        let settled = message.settle(
            vec![("b@y".to_owned(), Outcome::Deferred("busy".to_owned()))],
            now,
        );
        message = settled.pending.expect("still queued");
        delays.push(message.next_attempt_at - now);
        now = message.next_attempt_at;
    }
    assert_eq!(delays, [60, 300, 900, 3_600, 14_400, 14_400, 14_400]);
    assert_eq!(message.last_error.as_deref(), Some("busy"));
    assert!(!message.is_due(now - 1) && message.is_due(now));
}

#[test]
fn test_failures_and_old_deferrals_bounce() {
    let settled = pending(&["b@y", "c@z", "d@w"]).settle(
        vec![
            ("b@y".to_owned(), Outcome::Failed("no such user".to_owned())),
            ("c@z".to_owned(), Outcome::Deferred("busy".to_owned())),
            ("d@w".to_owned(), Outcome::Delivered),
        ],
        10,
    );
    assert_eq!(settled.bounced, vec![(
        "b@y".to_owned(),
        "no such user".to_owned()
    )]);
    assert_eq!(
        settled.pending.map(|p| p.recipients),
        Some(vec!["c@z".to_owned()])
    );

    let expired = pending(&["c@z"]).settle(
        vec![("c@z".to_owned(), Outcome::Deferred("busy".to_owned()))],
        MAX_AGE_SECS,
    );
    assert_eq!(expired.pending, None);
    assert_eq!(expired.bounced.len(), 1);
}
