//! Wasm Memory utils for loading witness wasm
//! =======================================
//! Copyright (c) 2021 Georgios Konstantopoulos
//! Copyright (c) Lurk Lab
//! Copyright (c) 2024 Rings Network
//! SPDX-License-Identifier: MIT
//!
//! Contributors:
//!
//! - Hanting Zhang (winston@lurk-lab.com)
//!   - Adapted the original work here: https://!github.com/arkworks-rs/circom-compat/blob/master/src/witness/memory.rs
//!   - Retrofitted for support without `arkworks` libraries such as `ark-ff` or `ark-bignum`, which were replaced with `ff` and `crypto-bignum`.
//!
//!
//! - Rings Network Dev Team
//!   - refine the original work here: <https://github.com/lurk-lab/circom-scotia/tree/main/src/witness>
//!

use std::ops::Deref;

use crypto_bigint::Encoding;
use crypto_bigint::U256;
use ff::PrimeField;
use wasmer::AsStoreRef;
use wasmer::Memory;
use wasmer::MemoryView;

use super::calculator::from_vec_u32;
use super::calculator::u256_to_vec_u32;
use crate::error::Result;

/// A wrapper around WebAssembly memory to facilitate safe and convenient memory operations.
/// Provides methods to read and write various data types, especially those related to cryptographic computations.
#[derive(Clone, Debug)]
pub struct SafeMemory {
    /// The underlying WebAssembly memory object.
    pub memory: Memory,
    /// The prime field used in cryptographic computations.
    pub prime: U256,
    /// Internal fields for handling numerical bounds and memory sizes.
    short_max: U256,
    short_min: U256,
    n32: usize,
}

/// Enables dereferencing to the underlying `Memory` object for direct access.
impl Deref for SafeMemory {
    type Target = Memory;

    fn deref(&self) -> &Self::Target {
        &self.memory
    }
}

impl SafeMemory {
    /// Constructs a new `SafeMemory` object given WebAssembly memory, size of field elements, and the prime field.
    pub fn new(memory: Memory, n32: usize, prime: U256) -> Self {
        // TODO: Figure out a better way to calculate these
        let short_max = U256::from(0x8000_0000u64);
        let short_min = short_max.neg_mod(&prime);

        Self {
            memory,
            prime,
            short_max,
            short_min,
            n32,
        }
    }

    /// Gets an immutable view to the memory in 32 byte chunks
    pub fn view<'a>(&self, store: &'a impl AsStoreRef) -> MemoryView<'a> {
        self.memory.view(store)
    }

    /// Returns the next free position in the memory
    pub fn free_pos(&self, store: &impl AsStoreRef) -> u32 {
        self.read_u32(store, 0)
    }

    /// Sets the next free position in the memory
    pub fn set_free_pos(&mut self, store: &impl AsStoreRef, ptr: u32) {
        self.write_u32(store, 0, ptr);
    }

    /// Allocates a U32 in memory
    pub fn alloc_u32(&mut self, store: &impl AsStoreRef) -> u32 {
        let p = self.free_pos(store);
        self.set_free_pos(store, p + 8);
        p
    }

    /// Writes a u32 to the specified memory offset
    pub fn write_u32(&mut self, store: &impl AsStoreRef, ptr: usize, num: u32) {
        let view = self.view(store);
        let buf = unsafe { view.data_unchecked_mut() };
        buf[ptr..ptr + std::mem::size_of::<u32>()].copy_from_slice(&num.to_le_bytes());
    }

    /// Reads a u32 from the specified memory offset
    pub fn read_u32(&self, store: &impl AsStoreRef, ptr: usize) -> u32 {
        let view = self.view(store);
        let buf = unsafe { view.data_unchecked() };

        let mut bytes = [0; 4];
        bytes.copy_from_slice(&buf[ptr..ptr + std::mem::size_of::<u32>()]);

        u32::from_le_bytes(bytes)
    }

    /// Allocates `self.n32 * 4 + 8` bytes in the memory
    pub fn alloc_fr(&mut self, store: &impl AsStoreRef) -> u32 {
        let p = self.free_pos(store);
        self.set_free_pos(store, p + self.n32 as u32 * 4 + 8);
        p
    }

    /// Writes a Field Element to memory at the specified offset, truncating
    /// to smaller u32 types if needed and adjusting the sign via 2s complement
    pub fn write_fr(&mut self, store: &impl AsStoreRef, ptr: usize, fr: U256) -> Result<()> {
        if fr < self.short_max && fr > self.short_min {
            self.write_short(store, ptr, fr)?;
        } else {
            self.write_long_normal(store, ptr, fr)?;
        }

        Ok(())
    }

    /// Reads a Field Element from the memory at the specified offset
    pub fn read_fr<F: PrimeField>(&self, store: &impl AsStoreRef, ptr: usize) -> F {
        let view = self.view(store);
        let view = unsafe { view.data_unchecked_mut() };

        if view[ptr + 7] & 0x80 != 0 {
            let num = self.read_big(store, ptr + 8);
            from_vec_u32(u256_to_vec_u32(num))
        } else {
            F::from(u64::from(self.read_u32(store, ptr)))
        }
    }

    fn write_short(&mut self, store: &impl AsStoreRef, ptr: usize, fr: U256) -> Result<()> {
        let num = fr.to_words()[0] as u32;
        self.write_u32(store, ptr, num);
        self.write_u32(store, ptr + 4, 0);
        Ok(())
    }

    fn write_long_normal(&mut self, store: &impl AsStoreRef, ptr: usize, fr: U256) -> Result<()> {
        self.write_u32(store, ptr, 0);
        self.write_u32(store, ptr + 4, i32::MIN as u32); // 0x80000000
        self.write_big(store, ptr + 8, fr)?;
        Ok(())
    }

    fn write_big(&self, store: &impl AsStoreRef, ptr: usize, num: U256) -> Result<()> {
        let view = self.view(store);
        let buf = unsafe { view.data_unchecked_mut() };

        let bytes: [u8; 32] = num.to_le_bytes();
        buf[ptr..ptr + 32].copy_from_slice(&bytes);

        Ok(())
    }

    /// Reads `num_bytes * 32` from the specified memory offset in a Big Integer
    pub fn read_big(&self, store: &impl AsStoreRef, ptr: usize) -> U256 {
        let view = self.view(store);
        let buf = unsafe { view.data_unchecked() };

        U256::from_le_slice(&buf[ptr..])
    }
}
