use std::time::Duration;

use rand::distributions::Distribution;

use super::DUMMY_DELAY_MAX;
use super::DUMMY_DELAY_MIN;

pub(super) async fn random_delay() {
    tokio::time::sleep(Duration::from_millis(random(
        DUMMY_DELAY_MIN,
        DUMMY_DELAY_MAX,
    )))
    .await;
}

pub(super) fn random(low: u64, high: u64) -> u64 {
    let range = rand::distributions::Uniform::new(low, high);
    let mut rng = rand::thread_rng();
    range.sample(&mut rng)
}
