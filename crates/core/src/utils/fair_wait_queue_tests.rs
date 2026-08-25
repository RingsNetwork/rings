use std::task::Context;
use std::task::Poll;

use futures::task::noop_waker;

use super::*;

fn queue(max_waiters: usize) -> Arc<FairWaitQueue> {
    Arc::new(FairWaitQueue::with_budget(Arc::new(FairWaitBudget::new(
        max_waiters,
        max_waiters,
    ))))
}

fn blocked_waiter(queue: &Arc<FairWaitQueue>) -> FairWaiter {
    match queue
        .admit_or_wait(1, (), || None::<()>)
        .expect("one waiter must fit the test budget")
    {
        FairAdmission::Ready(()) => panic!("a blocked attempt must enqueue"),
        FairAdmission::Waiting(waiter) => waiter,
    }
}

fn poll_waiter(waiter: &mut FairWaiter, value: usize) -> Poll<Option<usize>> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    waiter.poll(&mut context, || Some(value))
}

#[test]
fn wake_before_first_poll_is_retained() {
    let queue = queue(1);
    let mut waiter = blocked_waiter(&queue);

    queue.wake_front();

    assert_eq!(poll_waiter(&mut waiter, 7), Poll::Ready(Some(7)));
}

#[test]
fn cancelling_armed_front_hands_wake_to_successor() {
    let queue = queue(2);
    let first = blocked_waiter(&queue);
    let mut second = blocked_waiter(&queue);
    queue.wake_front();

    drop(first);

    assert_eq!(poll_waiter(&mut second, 2), Poll::Ready(Some(2)));
}

#[test]
fn cancelling_middle_preserves_fifo_release_order() {
    let queue = queue(3);
    let mut first = blocked_waiter(&queue);
    let middle = blocked_waiter(&queue);
    let mut third = blocked_waiter(&queue);
    drop(middle);
    queue.wake_front();

    assert_eq!(poll_waiter(&mut third, 3), Poll::Pending);
    assert_eq!(poll_waiter(&mut first, 1), Poll::Ready(Some(1)));
    assert_eq!(poll_waiter(&mut third, 3), Poll::Ready(Some(3)));
}

#[test]
fn cancelled_waiter_releases_shared_budget() {
    let queue = queue(1);
    let waiter = blocked_waiter(&queue);
    assert!(queue.admit_or_wait(1, (), || None::<()>).is_err());

    drop(waiter);

    assert!(matches!(
        queue.admit_or_wait(1, (), || None::<()>),
        Ok(FairAdmission::Waiting(_))
    ));
}
