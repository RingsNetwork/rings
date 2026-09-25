//! The benchmark gate of #840: the cost of peeling one cell at a relay, natively and on `wasm32`.
//!
//! One cell is everything a relay computes from the received bytes to the bytes it forwards:
//!
//! ```text
//! parse   |w| ↦ (b, χ, y)
//! header  α check, ECDH d·α, HKDF key schedule (blinding factor z by wide reduction), ChaCha20
//!         ρ over (Ĥ+1)ℓ, HMAC γ over b ‖ β (b the class), layer decode, α′ = z·α
//! carry   KDF₄₈(σ_in), AEZ key setup, one AEZ decipherment of C_16KiB = 13465 bytes
//! encode  χ′ ‖ y′
//! ```
//!
//! Beside the two measurements of the real code paths (header peel, whole cell), the same
//! primitives are timed alone with the operands of one cell, so that the cost splits into
//! ECDH, blinding, key schedule, PRG, MAC and AEZ. Every row runs `WARM_UP` untimed passes and
//! then `RUNS` timed passes over state built outside the timed region, on both targets alike, by
//! the monotonic clock of `web_time::Instant` (`std::time::Instant` natively, `performance.now()`
//! in the browser); every result passes through `black_box`.
//!
//! The target is ≥ 109 cells/s per neighbour on `wasm32` (#834 L9). The numbers are reported,
//! never asserted: a duration is a measurement, not a law, so the benchmarks are ignored by
//! default and run on request.
//!
//! ```text
//! native:  cargo test --release -p rings-node --lib sphinx::tests::test_peel_cost -- --ignored \
//!            --nocapture --test-threads=1
//! wasm32:  CHROMEDRIVER=… cargo test --release -p rings-node --lib \
//!            --target wasm32-unknown-unknown --features browser_default --no-default-features \
//!            -- --include-ignored --nocapture bench_peel_cost
//! ```

use core::hint::black_box;

use chacha20::cipher::KeyIvInit;
use chacha20::cipher::StreamCipher;
use chacha20::ChaCha20;
use hkdf::HkdfExtract;
use hmac::Hmac;
use hmac::Mac;
use rand::rngs::StdRng;
use rand::RngCore;
use rings_aez::Tweak;
use rings_core::delegation::DelegateeKey;
use rings_core::ecc::prime_order::NonIdentityPoint;
use rings_core::ecc::prime_order::NonZeroScalar;
use rings_core::ecc::Secp256k1;
use sha2::Sha256;
use web_time::Instant;

use super::fixture_keys;
use super::fixture_rng;
use super::fixture_route;
use crate::onion::loop_shape::MAX_ONION_LOOP_HOPS;
use crate::onion::sphinx::cell::OnionCell;
use crate::onion::sphinx::cell::OnionStep;
use crate::onion::sphinx::class::OnionLoopClass;
use crate::onion::sphinx::header::OnionHeader;
use crate::onion::sphinx::header::ONION_HEADER_ROUTING_BYTES;
use crate::onion::sphinx::layer::ONION_LAYER_BYTES;
use crate::onion::sphinx::seed::OnionCarrySeed;
use crate::onion::sphinx::seed::OnionSegmentSeed;

/// Untimed passes per row before measuring.
const WARM_UP: u32 = 200;

/// Timed passes per row.
const RUNS: u32 = 2000;

/// Mean microseconds per pass of `run`, after `WARM_UP` untimed passes.
fn microseconds_per_run(mut run: impl FnMut()) -> f64 {
    (0..WARM_UP).for_each(|_| run());
    let start = Instant::now();
    (0..RUNS).for_each(|_| run());
    start.elapsed().as_secs_f64() * 1_000_000.0 / f64::from(RUNS)
}

/// Every row of the report: `(operation, µs per pass)`, components first, then the real paths.
fn measure() -> Vec<(&'static str, f64)> {
    let mut rng = fixture_rng(40);
    let class = OnionLoopClass::DEFAULT;
    let keys = fixture_keys(MAX_ONION_LOOP_HOPS);
    let (header, _) =
        OnionHeader::build(&fixture_route(40, &keys), class, &mut rng).expect("build the header");
    let key = &keys[0];
    let peeled = header.peel(class, key).expect("peel");
    let (_, segment) = OnionSegmentSeed::draw(&mut rng).expect("strong segment");
    let (cell, _) = OnionCell::client(
        &fixture_route(40, &keys),
        class,
        &segment,
        b"value",
        &mut rng,
    )
    .expect("client cell");
    // `parse` consumes its buffer, so every pass gets its own copy, made before timing starts.
    let passes = usize::try_from(WARM_UP + RUNS).expect("pass count fits usize");
    let cells = vec![cell.into_bytes(); passes];

    let mut rows = component_rows(key, &peeled.layer.inbound, &mut rng);
    rows.extend(real_path_rows(key, &header, cells));
    rows
}

/// The rows that time each primitive of one cell alone, with that cell's operands.
fn component_rows(
    key: &DelegateeKey,
    inbound: &OnionCarrySeed,
    rng: &mut StdRng,
) -> Vec<(&'static str, f64)> {
    let class = OnionLoopClass::DEFAULT;
    let carry_key = inbound.key().expect("strong key");
    let mut slot = vec![0; class.carry_bytes()];
    rng.fill_bytes(&mut slot);
    let alpha = NonIdentityPoint::generator_mul(&NonZeroScalar::<Secp256k1>::random_with_rng(rng));
    let blinding = NonZeroScalar::<Secp256k1>::random_with_rng(rng);
    let mut stream = vec![0_u8; ONION_HEADER_ROUTING_BYTES + ONION_LAYER_BYTES];
    let mut routing = vec![0_u8; ONION_HEADER_ROUTING_BYTES];
    rng.fill_bytes(&mut routing);
    let mut okm = [0_u8; 64];

    vec![
        (
            "ECDH d·α",
            microseconds_per_run(|| {
                black_box(key.diffie_hellman(black_box(&alpha)));
            }),
        ),
        (
            "blind z·α",
            microseconds_per_run(|| {
                black_box(black_box(&alpha) * black_box(&blinding));
            }),
        ),
        (
            "HKDF schedule",
            microseconds_per_run(|| {
                let mut extract = HkdfExtract::<Sha256>::new(Some(b"salt".as_slice()));
                extract.input_ikm(black_box([7_u8; 65].as_slice()));
                let (_, kdf) = extract.finalize();
                for info in [b"prg".as_slice(), b"mac", b"blind"] {
                    kdf.expand(info, &mut okm).expect("HKDF length");
                }
                black_box(&okm);
            }),
        ),
        (
            "PRG ChaCha20 (Ĥ+1)ℓ",
            microseconds_per_run(|| {
                ChaCha20::new(&[7_u8; 32].into(), &[0_u8; 12].into())
                    .apply_keystream(black_box(stream.as_mut_slice()));
                black_box(&stream);
            }),
        ),
        (
            "MAC HMAC-SHA256 b ‖ β",
            microseconds_per_run(|| {
                let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&[7_u8; 64]).expect("HMAC key");
                mac.update(&class.mac_label());
                mac.update(black_box(routing.as_slice()));
                black_box(mac.finalize());
            }),
        ),
        (
            "carry key KDF₄₈ + AEZ setup",
            microseconds_per_run(|| {
                black_box(black_box(inbound).key().expect("strong key"));
            }),
        ),
        (
            "AEZ decipher C_16KiB",
            microseconds_per_run(|| {
                carry_key
                    .aez()
                    .decipher(Tweak::EMPTY, black_box(slot.as_mut_slice()));
            }),
        ),
    ]
}

/// The rows that time the real code paths: the header peel alone, and a whole relayed cell from
/// the received bytes to the forwarded bytes, one prepared cell per pass.
fn real_path_rows(
    key: &DelegateeKey,
    header: &OnionHeader,
    cells: Vec<Vec<u8>>,
) -> Vec<(&'static str, f64)> {
    let class = OnionLoopClass::DEFAULT;
    let mut cells = cells.into_iter();
    vec![
        (
            "header peel (real path)",
            microseconds_per_run(|| {
                black_box(black_box(header).peel(class, key).expect("peel"));
            }),
        ),
        (
            "whole cell: parse, peel, relay, encode",
            microseconds_per_run(|| {
                let bytes = cells.next().expect("one prepared cell per pass");
                let peeled = OnionCell::parse(black_box(bytes))
                    .expect("cell")
                    .peel(key)
                    .expect("peel");
                black_box(match peeled.step().expect("relay step") {
                    OnionStep::Relayed { cell, .. } => cell.into_bytes(),
                    OnionStep::Consumed { .. } => Vec::new(),
                });
            }),
        ),
    ]
}

/// Renders a measurement as a table, one row per operation, in µs and passes per second.
fn report(target: &str, rows: &[(&'static str, f64)]) -> String {
    rows.iter().fold(
        format!("peel cost ({target}, {RUNS} runs per row after {WARM_UP} warm-up runs)"),
        |table, (operation, micros)| {
            format!(
                "{table}\n  {operation:<40} {micros:>9.1} µs  {:>9.0} /s",
                1_000_000.0 / micros
            )
        },
    )
}

/// Native per-cell peel cost.
#[cfg(not(target_family = "wasm"))]
#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture --test-threads=1"]
fn bench_peel_cost_native() {
    println!("{}", report("native", &measure()));
}

/// `wasm32` per-cell peel cost in the browser.
#[cfg(rings_browser)]
#[wasm_bindgen_test::wasm_bindgen_test]
#[ignore = "benchmark: run with --release -- --include-ignored --nocapture"]
fn bench_peel_cost_wasm() {
    wasm_bindgen_test::console_log!("{}", report("wasm32", &measure()));
}
