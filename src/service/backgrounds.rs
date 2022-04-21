use rings_core::dht::Stabilization;

use anyhow::{anyhow, Result};
use tokio::time::Duration;

#[allow(dead_code)]
pub async fn run_stabilize(stabilization: Stabilization) {
    let mut result = Result::<()>::Ok(());
    let timeout_in_secs = stabilization.get_timeout();
    while result.is_ok() {
        let timeout = tokio::time::sleep(Duration::from_secs(timeout_in_secs as u64));
        tokio::pin!(timeout);
        tokio::select! {
            _ = timeout.as_mut() => {
                result = stabilization.stabilize().await.map_err(|e| anyhow!("{:?}", e));
            }
        }
    }
}
