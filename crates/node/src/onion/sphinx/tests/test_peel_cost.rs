//! The benchmark gate of #840: the cost of peeling one cell at a relay, natively and on `wasm32`.
//!
//! One cell is everything a relay computes before it forwards:
//!
//! ```text
//! header  α check, ECDH d·α, HKDF key schedule (blinding b by wide reduction), ChaCha20 ρ over
//!         (Ĥ+1)ℓ, HMAC γ over β, layer decode, α′ = b·α
//! carry   KDF₄₈(σ_in), AEZ key setup, one AEZ decipherment of C_16KiB = 13241 bytes
//! ```
//!
//! The target is ≥ 109 cells/s per neighbour on `wasm32` (#834 L9). The numbers are reported,
//! never asserted: a duration is a measurement, not a law, so the benchmarks are ignored by
//! default and run on request.
//!
//! ```text
//! native:  cargo test --release -p rings-node --lib sphinx::tests::test_peel_cost -- --ignored --nocapture
//! wasm32:  CHROMEDRIVER=… cargo test --release -p rings-node --lib --target wasm32-unknown-unknown \
//!            --features browser_chrome_test --no-default-features -- --include-ignored bench_peel_cost
//! ```

use rand::RngCore;
use rings_core::utils::get_epoch_ms;

use super::fixture_loop;
use super::fixture_rng;
use crate::onion::sphinx::carry::OnionCarry;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::MAX_ONION_LOOP_HOPS;

/// Cells peeled per measurement.
const CELLS: u32 = 1000;

/// The measured throughputs, in cells per second.
struct PeelCost {
    /// Header peel alone.
    header: f64,
    /// Carry step alone.
    carry: f64,
    /// One whole cell: header peel, then the carry step under the peeled layer's seed.
    cell: f64,
}

/// `CELLS / elapsed` for `CELLS` runs of `step`, by the wall clock.
fn cells_per_second(mut step: impl FnMut()) -> f64 {
    let start = get_epoch_ms();
    (0..CELLS).for_each(|_| step());
    let elapsed_ms = get_epoch_ms().saturating_sub(start).max(1);
    f64::from(CELLS) * 1000.0 / elapsed_ms as f64
}

/// Peel the first header of a longest loop, and a class-16KiB carry, `CELLS` times each.
fn measure() -> PeelCost {
    let mut rng = fixture_rng(40);
    let (keys, _, route) = fixture_loop(&mut rng, MAX_ONION_LOOP_HOPS);
    let header = OnionHeader::build(&route, &mut rng).expect("build the header");
    let key = &keys[0];
    let inbound = header.peel(key).expect("peel").layer.inbound.clone();
    let class = OnionLoopClass::KiB16;
    let mut slot = vec![0; class.carry_bytes()];
    rng.fill_bytes(&mut slot);
    let mut carry = OnionCarry::from_bytes(class, slot).expect("carry width");

    let header_rate = cells_per_second(|| {
        header.peel(key).expect("peel");
    });
    let carry_rate = cells_per_second(|| {
        carry = carry.clone().peel(&inbound.key().expect("strong key"));
    });
    let cell_rate = cells_per_second(|| {
        let peeled = header.peel(key).expect("peel");
        carry = carry
            .clone()
            .peel(&peeled.layer.inbound.key().expect("strong key"));
    });
    PeelCost {
        header: header_rate,
        carry: carry_rate,
        cell: cell_rate,
    }
}

/// Renders a measurement as one report line.
fn report(target: &str, cost: &PeelCost) -> String {
    format!(
        "peel cost ({target}, {CELLS} cells): header {:.0} cells/s, carry {:.0} cells/s, \
         cell {:.0} cells/s ({:.2} ms/cell)",
        cost.header,
        cost.carry,
        cost.cell,
        1000.0 / cost.cell
    )
}

/// Native per-cell peel cost.
#[cfg(not(target_family = "wasm"))]
#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn bench_peel_cost_native() {
    println!("{}", report("native", &measure()));
}

/// `wasm32` per-cell peel cost in the browser.
#[cfg(rings_browser)]
#[wasm_bindgen_test::wasm_bindgen_test]
#[ignore = "benchmark: run with --release -- --include-ignored"]
fn bench_peel_cost_wasm() {
    wasm_bindgen_test::console_log!("{}", report("wasm32", &measure()));
}
