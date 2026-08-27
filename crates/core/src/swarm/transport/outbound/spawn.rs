use super::MeasurementReceiver;
use super::OutboundWorker;
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
use crate::error::Error;
use crate::error::Result;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub(super) fn spawn_worker(
    worker: OutboundWorker,
    measurements: MeasurementReceiver,
) -> Result<()> {
    wasm_bindgen_futures::spawn_local(worker.run());
    wasm_bindgen_futures::spawn_local(measurements.run());
    Ok(())
}

#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub(super) fn spawn_worker(
    worker: OutboundWorker,
    measurements: MeasurementReceiver,
) -> Result<()> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| Error::OutboundSchedulerRuntimeUnavailable)?;
    runtime.spawn(worker.run());
    runtime.spawn(measurements.run());
    Ok(())
}
