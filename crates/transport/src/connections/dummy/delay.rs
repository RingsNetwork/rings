use std::time::Duration;

use rand::distributions::Distribution;

use super::CONTROLLED_RNG_STATE;
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
    if let Some(value) = controlled_random(low, high) {
        return value;
    }
    let range = rand::distributions::Uniform::new(low, high);
    let mut rng = rand::thread_rng();
    range.sample(&mut rng)
}

fn controlled_random(low: u64, high: u64) -> Option<u64> {
    CONTROLLED_RNG_STATE.with(|state| {
        let mut current = state.get()?;
        current = mix_seed(current);
        state.set(Some(current));
        Some(low + current % high.saturating_sub(low).max(1))
    })
}

/// Mix one seed with the dummy runtime's canonical SplitMix64 function.
pub const fn mix_seed(state: u64) -> u64 {
    let mut value = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}
