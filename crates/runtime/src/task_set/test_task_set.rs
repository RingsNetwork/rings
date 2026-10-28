//! The liveness-count law of [`TaskSet`], with every wait woken by a task's end.

use futures::channel::oneshot;
use futures::FutureExt;

use super::TaskSet;
use crate::task::Abandoned;

/// A set with no task is drained at once.
#[test]
fn test_an_empty_set_is_drained() {
    assert_eq!(TaskSet::new().drained().now_or_never(), Some(()));
}

/// The drain waits for every live task, a child a live task spawned into the set included, and
/// is woken by the end of the last; each task's output reaches its `Joined`.
#[tokio::test]
async fn test_the_drain_waits_for_every_live_task_and_its_children() {
    let set = TaskSet::new();
    let (release_first, first_gate) = oneshot::channel::<()>();
    let (release_child, child_gate) = oneshot::channel::<()>();
    let parent_set = set.clone();
    let first = set
        .spawn(async move {
            let child = parent_set
                .spawn(async move { child_gate.await.map(|()| "child") })
                .map_err(|_| Abandoned);
            first_gate.await.map(|()| child)
        })
        .map_err(|_| Abandoned);
    let mut drained = set.drained();
    assert_eq!((&mut drained).now_or_never(), None);

    let _ = release_first.send(());
    let child = first
        .expect("a runtime is current")
        .await
        .expect("the first task ends")
        .expect("its gate was released")
        .expect("a runtime is current");
    assert_eq!(set.live(), 1);
    assert_eq!((&mut drained).now_or_never(), None);

    let _ = release_child.send(());
    assert_eq!(child.await, Ok(Ok("child")));
    drained.await;
    assert_eq!(set.live(), 0);
}

/// A task its executor drops before it ends leaves the set: its `Joined` is `Abandoned`, and
/// the drain resolves.
#[test]
fn test_a_task_dropped_with_its_runtime_leaves_the_set() {
    let set = TaskSet::new();
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let joined = {
        let _entered = runtime.enter();
        set.spawn(std::future::pending::<()>())
            .map_err(|_| Abandoned)
            .expect("a runtime is current")
    };
    assert_eq!(set.live(), 1);

    drop(runtime);

    assert_eq!(set.live(), 0);
    assert_eq!(set.drained().now_or_never(), Some(()));
    assert_eq!(futures::executor::block_on(joined), Err(Abandoned));
}

/// Outside an executor a task is handed back unpolled, and the set is unchanged.
#[test]
fn test_a_task_outside_an_executor_is_handed_back() {
    let set = TaskSet::new();
    let unscheduled = set
        .spawn(async { 7 })
        .err()
        .expect("no runtime is current outside Tokio");

    assert_eq!(set.live(), 0);
    assert_eq!(futures::executor::block_on(unscheduled.into_inner()), 7);
}
