use rings_runtime::Spawner;

use super::MeasurementReceiver;
use super::OutboundWorker;
use crate::error::Error;
use crate::error::Result;

/// Run the outbound worker and its measurement drain on the current runtime.
///
/// Post: `Err` iff no runtime is current, in which case neither task was started.
pub(super) fn spawn_worker(
    worker: OutboundWorker,
    measurements: MeasurementReceiver,
) -> Result<()> {
    let spawner = Spawner::current().map_err(|_| Error::OutboundSchedulerRuntimeUnavailable)?;
    spawner.spawn(worker.run());
    spawner.spawn(measurements.run());
    Ok(())
}
