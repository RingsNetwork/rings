//! The liveness-count law of [`TaskSet`], with every wait woken by a task's end.

#[cfg(not(target_family = "wasm"))]
use futures::channel::oneshot;
use futures::FutureExt;

use super::TaskSet;
#[cfg(any(not(target_family = "wasm"), feature = "browser"))]
use crate::task::Abandoned;

/// A set with no task is drained at once.
#[test]
fn test_an_empty_set_is_drained() {
    assert_eq!(TaskSet::new().drained().now_or_never(), Some(()));
}

#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
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

#[cfg(all(feature = "browser", target_family = "wasm"))]
mod browser_tests {
    use futures::channel::oneshot;
    use futures::FutureExt;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::Abandoned;
    use super::TaskSet;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn test_browser_task_set_drains_after_spawned_tasks() {
        let set = TaskSet::new();
        let (release, gate) = oneshot::channel::<()>();
        let joined = set
            .spawn(async move { gate.await.map(|()| 42) })
            .map_err(|_| Abandoned)
            .unwrap();
        let mut drained = set.drained();
        assert_eq!((&mut drained).now_or_never(), None);
        assert_eq!(set.live(), 1);

        let _ = release.send(());
        assert_eq!(joined.await, Ok(Ok(42)));
        drained.await;
        assert_eq!(set.live(), 0);
    }
}
