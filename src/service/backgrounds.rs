use bns_core::dht::Stabilization;

use anyhow::Result;
use tokio::time::Duration;

pub async fn run_stabilize(stabilization: Stabilization) {
    let mut result = Result::<()>::Ok(());
    let timeout_in_secs = stabilization.get_timeout();
    while result.is_ok() {
        let timeout = tokio::time::sleep(Duration::from_secs(timeout_in_secs as u64));
        tokio::pin!(timeout);
        tokio::select! {
            _ = timeout.as_mut() => {
                result = stabilization.stabilize().await;
            }
        }
    }
}
