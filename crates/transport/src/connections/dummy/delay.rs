use rand::distributions::Distribution;

use super::ARRIVAL_TURNS;
use super::CONTROLLED_RNG_STATE;

/// Let the other ready tasks run before a frame arrives: a number of scheduler turns drawn
/// uniformly from `0..ARRIVAL_TURNS` by [`random`], so a seeded test replays the same draws.
///
/// Law: no clock elapses. Per link, frames arrive in send order, since one listener handles a
/// connection's events in turn; across links, a frame may be overtaken by up to
/// `ARRIVAL_TURNS - 1` turns of every other ready task, so links race as they do over a
/// network, while no wait's length depends on host load.
pub(super) async fn yield_before_arrival() {
    for _ in 0..random(0, ARRIVAL_TURNS) {
        tokio::task::yield_now().await;
    }
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
