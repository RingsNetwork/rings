//! Canonical runtime adapter for detached extension tasks.

use std::future::Future;
use std::time::Duration;

use futures::channel::oneshot;

use crate::error::Error;
use crate::error::Result;

/// Spawn owned extension work without tying its completion to the current protocol turn.
///
/// Native tasks must be `Send`; browser tasks remain on the single-threaded wasm executor.
#[cfg(rings_native)]
pub(crate) fn spawn_detached<F>(future: F)
where F: Future<Output = ()> + Send + 'static {
    tokio::spawn(future);
}

/// Browser counterpart of [`spawn_detached`].
#[cfg(rings_browser)]
pub(crate) fn spawn_detached<F>(future: F)
where F: Future<Output = ()> + 'static {
    wasm_bindgen_futures::spawn_local(future);
}

/// Run owned work independently of the lifetime of the future waiting for its result.
#[cfg(rings_native)]
pub(crate) async fn run_detached<F, T>(future: F) -> Result<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    spawn_detached(async move {
        let output = future.await;
        let _ = sender.send(output);
    });
    receiver
        .await
        .map_err(|_| Error::DetachedExtensionTaskClosed)
}

/// Browser counterpart of [`run_detached`].
#[cfg(rings_browser)]
pub(crate) async fn run_detached<F, T>(future: F) -> Result<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    let (sender, receiver) = oneshot::channel();
    spawn_detached(async move {
        let output = future.await;
        let _ = sender.send(output);
    });
    receiver
        .await
        .map_err(|_| Error::DetachedExtensionTaskClosed)
}

/// Wait without exposing a target-specific timer to protocol interpreters.
#[cfg(rings_native)]
pub(crate) async fn sleep(duration: Duration) -> Result<()> {
    futures_timer::Delay::new(duration).await;
    Ok(())
}

/// Browser and worker counterpart of [`sleep`].
#[cfg(rings_browser)]
pub(crate) async fn sleep(duration: Duration) -> Result<()> {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    rings_core::utils::js_utils::window_sleep(millis)
        .await
        .map(|_| ())
        .map_err(|error| crate::error::Error::JsError(format!("{error:?}")))
}

#[cfg(all(test, rings_native))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::run_detached;
    use super::Error;
    use super::Result;

    #[tokio::test]
    async fn test_cancelling_waiter_does_not_cancel_owned_detached_work() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let waiter = {
            let started = started.clone();
            let release = release.clone();
            let completed = completed.clone();
            tokio::spawn(run_detached(async move {
                started.notify_one();
                release.notified().await;
                completed.notify_one();
            }))
        };

        started.notified().await;
        waiter.abort();
        let _ = waiter.await;
        release.notify_one();

        tokio::time::timeout(Duration::from_secs(1), completed.notified())
            .await
            .expect("owned detached work must outlive its cancelled waiter");
    }

    #[tokio::test]
    async fn test_detached_task_failure_preserves_typed_closed_error() {
        let result: Result<()> = run_detached(async {
            panic!("injected detached extension task failure");
        })
        .await;

        assert!(matches!(result, Err(Error::DetachedExtensionTaskClosed)));
    }
}
