//! A Rust Module for loading and calculating witness for circom circuits from wasm.
//! ========================
//! - refine the original work here: <https://github.com/lurk-lab/circom-scotia/tree/main/src/witness>

pub mod calculator;
pub mod circom;
pub mod memory;

use std::hash::Hasher;

use fnv::FnvHasher;

pub(crate) fn fnv(inp: &str) -> (u32, u32) {
    let mut hasher = FnvHasher::default();
    hasher.write(inp.as_bytes());
    let h = hasher.finish();

    ((h >> 32) as u32, h as u32)
}
