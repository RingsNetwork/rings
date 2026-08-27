use std::collections::BinaryHeap;

use super::*;

fn is_woken(notifier: &Notifier) -> bool {
    notifier.state().woken
}

#[test]
fn test_scheduled_wake_heap_orders_earliest_deadline_first() {
    let early = Notifier::default();
    let late = Notifier::default();
    let now = Instant::now();

    let mut pending = BinaryHeap::new();
    pending.push(ScheduledWake::at(
        late.clone(),
        now + Duration::from_millis(100),
        0,
    ));
    pending.push(ScheduledWake::at(
        early.clone(),
        now + Duration::from_millis(10),
        1,
    ));

    pending.pop().unwrap().notifier.wake();
    assert!(is_woken(&early));
    assert!(!is_woken(&late));

    pending.pop().unwrap().notifier.wake();
    assert!(is_woken(&late));
}

#[test]
fn test_send_or_wake_falls_back_when_scheduler_is_missing_or_closed() {
    let missing_scheduler = Notifier::default();
    let request = ScheduledWake::new(missing_scheduler.clone(), 10_000);
    send_or_wake(missing_scheduler.clone(), request, None);
    assert!(is_woken(&missing_scheduler));

    let closed_scheduler = Notifier::default();
    let (sender, receiver) = mpsc::channel();
    drop(receiver);
    let request = ScheduledWake::new(closed_scheduler.clone(), 10_000);
    send_or_wake(closed_scheduler.clone(), request, Some(&sender));
    assert!(is_woken(&closed_scheduler));
}
