//! Witnesses of the AEZ laws. Every test runs natively under `cargo test` and in the browser
//! under `wasm-bindgen-test` (`wasm32-unknown-unknown`).

mod laws;
mod vectors;

#[cfg(target_arch = "wasm32")]
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Declares each `fn` as a test on both targets: `#[test]` natively and
/// `#[wasm_bindgen_test]` on `wasm32`.
macro_rules! witness {
    ($($(#[$meta:meta])* fn $name:ident() $body:block)*) => {
        $(
            $(#[$meta])*
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
            #[cfg_attr(not(target_arch = "wasm32"), test)]
            fn $name() $body
        )*
    };
}
pub(crate) use witness;

/// A reproducible byte stream (xorshift64) for sweeps; not a source of key material.
pub(crate) struct Stream(u64);

impl Stream {
    /// A stream fixed by `seed` (non-zero).
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next byte.
    pub(crate) fn byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }

    /// The next `length` bytes.
    pub(crate) fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.byte()).collect()
    }

    /// The next 48 bytes as a key.
    pub(crate) fn key(&mut self) -> [u8; crate::KEY_BYTES] {
        core::array::from_fn(|_| self.byte())
    }
}
