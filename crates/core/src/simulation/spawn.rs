//! Audited task-spawn boundary for deterministic simulation observers.

use std::sync::Arc;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::SimulationRuntimeError;

pub(crate) fn spawn_storage_progress_observer(
    notify: Arc<Notify>,
) -> Result<JoinHandle<()>, SimulationRuntimeError> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| SimulationRuntimeError::MissingTokioRuntime)?;
    Ok(runtime.spawn(async move {
        notify.notified().await;
        super::record_storage_progress();
    }))
}
