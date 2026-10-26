//! The laws of a node's tasks (`crate::tasks`): the stop ends every node task, one spawned
//! after it included, and the drain awaits each one's end, a winding-down task's own end too.
//! Every wait is woken by an event, never by time.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::channel::oneshot;
use futures::FutureExt;

use super::NodeTasks;

/// A task dropped at the stop yields `None`, and the drain resolves once it is gone.
#[tokio::test]
async fn test_the_stop_drops_a_node_task() {
    let tasks = NodeTasks::new();
    let joined = tasks.spawn(std::future::pending::<()>()).unwrap();
    assert!(
        tasks.drained().now_or_never().is_none(),
        "a live task holds the drain"
    );

    tasks.stop();

    assert_eq!(joined.await.ok(), Some(None));
    tasks.drained().await;
}

/// A task that ends on its own yields `Some` of its output.
#[tokio::test]
async fn test_a_node_task_that_ends_yields_its_output() {
    let tasks = NodeTasks::new();

    let joined = tasks.spawn(async { 7 }).unwrap();

    assert_eq!(joined.await.ok(), Some(Some(7)));
    tasks.drained().await;
}

/// A task spawned after the stop (as one started while a close is under way) ends at its first
/// suspension point, and a drain begun after its spawn awaits that end.
#[tokio::test]
async fn test_a_node_task_spawned_after_the_stop_ends_at_once() {
    let tasks = NodeTasks::new();
    tasks.stop();

    let joined = tasks.spawn(std::future::pending::<()>()).unwrap();

    assert_eq!(joined.await.ok(), Some(None));
    tasks.drained().await;
}

/// A winding-down task is not dropped at the stop: it observes it, finishes its own end, and
/// the drain resolves only after that end, here held until the test releases it.
#[tokio::test]
async fn test_the_drain_awaits_a_winding_down_task_to_its_end() {
    let tasks = NodeTasks::new();
    let spawner = tasks.spawner().unwrap();
    let (release, released) = oneshot::channel::<()>();
    let reset = Arc::new(AtomicBool::new(false));
    let witnessed = Arc::clone(&reset);
    let joined = spawner.spawn_winding_down(move |stop| async move {
        stop.stopped().await;
        let _ = released.await;
        witnessed.store(true, Ordering::Release);
    });

    tasks.stop();
    let mut drained = tasks.drained();
    assert!(
        (&mut drained).now_or_never().is_none(),
        "the drain waits for the task's own end"
    );
    release.send(()).unwrap();
    drained.await;

    assert!(reset.load(Ordering::Acquire), "the task finished its end");
    assert!(joined.await.is_ok());
}
