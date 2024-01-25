//! A Rust module for calculating witnesses for Circom circuits using WebAssembly (Wasm).
//! This module adapts the original work from arkworks-rs/circom-compat and refines it for use without specific `arkworks` libraries.
//! The module provides functionality to initialize Wasm instances, manage memory, and compute witnesses for circuit inputs.
//! ======================
//!
//! Copyright (c) 2021 Georgios Konstantopoulos
//! Copyright (c) Lurk Lab
//! Copyright (c) 2024 Rings Network
//!
//! SPDX-License-Identifier: MIT
//!
//! Contributors:
//!
//! - Hanting Zhang (winston@lurk-lab.com)
//!   - Adapted the original work here: https://!github.com/arkworks-rs/circom-compat/blob/master/src/witness/witness_calculator.rs
//!   - Retrofitted for support without `arkworks` libraries such as `ark-ff` or `ark-bignum`, which were replaced with `ff` and `crypto-bignum`.
//!
//! - Rings Network Dev Team
//!   - refine the original work here: <https://github.com/lurk-lab/circom-scotia/tree/main/src/witness>

use crypto_bigint::U256;
use ff::PrimeField;
use wasmer::imports;
use wasmer::AsStoreMut;
use wasmer::Function;
use wasmer::Instance;
use wasmer::Memory;
use wasmer::MemoryType;
use wasmer::Module;
use wasmer::RuntimeError;
use wasmer::Store;
#[cfg(feature = "llvm")]
use wasmer_compiler_llvm::LLVM;

use super::circom::Circom;
use super::circom::Circom2;
use super::circom::CircomBase;
use super::circom::Wasm;
use super::fnv;
use super::memory::SafeMemory;
use crate::error::Result;

/// A calculator for generating witness data from Circom circuits.
/// It leverages WebAssembly (Wasm) to execute Circom-generated Wasm code,
/// enabling the calculation of witness data for zero-knowledge proofs.
#[derive(Debug)]
pub struct WitnessCalculator {
    /// The Wasm instance of the Circom circuit.
    pub instance: Wasm,
    /// The store for the Wasm instance, managing memory and resources.
    pub store: Store,
    /// A wrapper around Wasm memory to ensure safe access.
    pub memory: SafeMemory,
    /// Number of 64-bit words used to represent the field element.
    pub n64: u32,
    /// Version of the Circom compiler used.
    pub circom_version: u32,
}

/// Error type to signal end of execution.
/// From <https://docs.wasmer.io/integrations/examples/exit-early>
/// Useful for early termination of the Wasm execution in case of errors.
#[derive(thiserror::Error, Debug, Clone, Copy)]
#[error("{0}")]
struct ExitCode(u32);

/// Little endian
/// Converts a vector of `u32` values to a field element in little endian format.
/// This function is crucial for translating Wasm memory data into field elements.
pub fn from_vec_u32<F: PrimeField>(arr: Vec<u32>) -> F {
    let mut res = F::ZERO;
    let radix = F::from(0x0001_0000_0000_u64);
    for &val in &arr {
        res = res * radix + F::from(u64::from(val));
    }
    res
}

/// Little endian
/// Converts a field element to a vector of `u32` values in little endian format.
/// Used for converting field elements into a format suitable for Wasm memory.
/// This function is available only when the `circom-2` feature is enabled.
pub fn to_vec_u32<F: PrimeField>(f: F) -> Vec<u32> {
    let repr = F::to_repr(&f);
    let repr = repr.as_ref();

    let (pre, res, suf) = unsafe { repr.align_to::<u32>() };
    assert_eq!(pre.len(), 0);
    assert_eq!(suf.len(), 0);

    res.into()
}

/// Little endian
/// Converts a slice of `u32` values to a `U256` number in little endian format.
/// Primarily used for handling cryptographic operations that require 256-bit integers.
pub fn u256_from_vec_u32(data: &[u32]) -> U256 {
    let mut limbs = [0u32; 8];
    limbs.copy_from_slice(data);

    cfg_if::cfg_if! {
        if #[cfg(target_pointer_width = "64")] {
            let (pre, limbs, suf) = unsafe { limbs.align_to::<u64>() };
            assert_eq!(pre.len(), 0);
            assert_eq!(suf.len(), 0);
            U256::from_words(limbs.try_into().unwrap())
        } else {
            U256::from_words(limbs.as_ref().try_into().unwrap())
        }
    }
}

/// Little endian
/// Converts a `U256` number to a vector of `u32` values in little endian format.
/// Facilitates the transfer of 256-bit integer data to Wasm memory.
pub fn u256_to_vec_u32(s: U256) -> Vec<u32> {
    let words = s.to_words();
    let (pre, res, suf) = unsafe { words.align_to::<u32>() };
    assert_eq!(pre.len(), 0);
    assert_eq!(suf.len(), 0);

    res.into()
}

impl WitnessCalculator {
    /// Creates a new `WitnessCalculator` from a file path to a Wasm moduLe.
    /// This is a convenience function that abstracts over `from_file`.
    pub fn new(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::from_file(path)
    }

    /// Creates a new `Store` for Wasm execution.
    pub fn new_store() -> Store {
        cfg_if::cfg_if! {
            if #[cfg(feature = "llvm")] {
                let compiler = LLVM::new();
                let store = Store::new(compiler);
            } else {

            }
        }
        Store::default()
    }

    /// Creates a `WitnessCalculator` from a file path to a Wasm module.
    /// Loads the module, initializes the memory and instance for witness calculation.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let store = Self::new_store();
        let module = Module::from_file(&store, path)?;
        Self::from_module(module, store)
    }

    /// Creates a `WitnessCalculator` from a Wasm `Module`.
    /// Sets up the necessary environment, such as memory and instance, for the module.
    pub fn from_module(module: Module, mut store: Store) -> Result<Self> {
        // Set up the memory
        let memory = Memory::new(&mut store, MemoryType::new(2000, None, false)).unwrap();
        let import_object = imports! {
            "env" => {
                "memory" => memory.clone(),
            },
            // Host function callbacks from the WASM
            "runtime" => {
                "error" => runtime::error(&mut store),
                "logSetSignal" => runtime::log_signal(&mut store),
                "logGetSignal" => runtime::log_signal(&mut store),
                "logFinishComponent" => runtime::log_component(&mut store),
                "logStartComponent" => runtime::log_component(&mut store),
                "log" => runtime::log_component(&mut store),
                "exceptionHandler" => runtime::exception_handler(&mut store),
                "showSharedRWMemory" => runtime::show_memory(&mut store),
                "printErrorMessage" => runtime::print_error_message(&mut store),
                "writeBufferMessage" => runtime::write_buffer_message(&mut store),
            }
        };
        let instance = Wasm::new(Instance::new(&mut store, &module, &import_object)?);
        let version = instance.get_version(&mut store).unwrap_or(1);

        fn new_circom2(
            mut store: Store,
            instance: Wasm,
            memory: Memory,
            version: u32,
        ) -> Result<WitnessCalculator> {
            let n32 = instance.get_field_num_len32(&mut store)?;
            let mut safe_memory = SafeMemory::new(memory, n32 as usize, U256::ZERO);
            instance.get_raw_prime(&mut store)?;
            let mut arr = vec![0; n32 as usize];
            for i in 0..n32 {
                let res = instance.read_shared_rw_memory(&mut store, i)?;
                arr[i as usize] = res;
            }
            let prime = u256_from_vec_u32(&arr);

            let n64 = ((prime.bits() - 1) / 64 + 1) as u32;
            safe_memory.prime = prime;

            Ok(WitnessCalculator {
                instance,
                store,
                memory: safe_memory,
                n64,
                circom_version: version,
            })
        }

        fn new_circom1(
            mut store: Store,
            instance: Wasm,
            memory: Memory,
            version: u32,
        ) -> Result<WitnessCalculator> {
            // Fallback to Circom 1 behavior
            let n32 = (instance.get_fr_len(&mut store)? >> 2) - 2;
            let mut safe_memory = SafeMemory::new(memory, n32 as usize, U256::ZERO);
            let ptr = instance.get_ptr_raw_prime(&mut store)?;
            let prime = safe_memory.read_big(&store, ptr as usize);

            let n64 = ((prime.bits() - 1) / 64 + 1) as u32;
            safe_memory.prime = prime;

            Ok(WitnessCalculator {
                instance,
                store,
                memory: safe_memory,
                n64,
                circom_version: version,
            })
        }

        match version {
            2 => new_circom2(store, instance, memory, version),
            1 => new_circom1(store, instance, memory, version),
            _ => panic!("Unknown Circom version"),
        }
    }

    /// Calculates the witness data for a given set of inputs.
    /// This function is the primary entry point for computing witness data.
    /// It handles different Circom versions and configurations.
    pub fn calculate_witness<F: PrimeField>(
        &mut self,
        input: Vec<(String, Vec<F>)>,
        sanity_check: bool,
    ) -> Result<Vec<F>> {
        self.instance.init(&mut self.store, sanity_check)?;

        match self.circom_version {
            2 => self.calculate_witness_circom2(input, sanity_check),
            1 => self.calculate_witness_circom1(input, sanity_check),
            _ => panic!("Unknown Circom version"),
        }
    }

    // Circom 1 default behavior
    fn calculate_witness_circom1<F: PrimeField>(
        &mut self,
        input: Vec<(String, Vec<F>)>,
        sanity_check: bool,
    ) -> Result<Vec<F>> {
        self.instance.init(&mut self.store, sanity_check)?;

        let old_mem_free_pos = self.memory.free_pos(&self.store);
        let p_sig_offset = self.memory.alloc_u32(&self.store);
        let p_fr = self.memory.alloc_fr(&self.store);

        // allocate the inputs
        for (name, values) in input {
            let (msb, lsb) = fnv(&name);

            self.instance
                .get_signal_offset32(&mut self.store, p_sig_offset, 0, msb, lsb)?;

            let sig_offset = self.memory.read_u32(&self.store, p_sig_offset as usize) as usize;

            for (i, _value) in values.into_iter().enumerate() {
                self.memory
                    .write_fr(&self.store, p_fr as usize, U256::ZERO)?; // TODO: FIXME
                self.instance
                    .set_signal(&mut self.store, 0, 0, (sig_offset + i) as u32, p_fr)?;
            }
        }

        let mut w = Vec::new();

        let n_vars = self.instance.get_n_vars(&mut self.store)?;
        for i in 0..n_vars {
            let ptr = self.instance.get_ptr_witness(&mut self.store, i)? as usize;
            let el = self.memory.read_fr(&self.store, ptr);
            w.push(el);
        }

        self.memory.set_free_pos(&self.store, old_mem_free_pos);

        Ok(w)
    }

    fn calculate_witness_circom2<F: PrimeField>(
        &mut self,
        input: Vec<(String, Vec<F>)>,
        sanity_check: bool,
    ) -> Result<Vec<F>> {
        self.instance.init(&mut self.store, sanity_check)?;

        let n32 = self.instance.get_field_num_len32(&mut self.store)?;

        // allocate the inputs
        for (name, values) in input {
            let (msb, lsb) = fnv(&name);

            for (i, value) in values.into_iter().enumerate() {
                let f_arr = to_vec_u32(value);
                for j in 0..n32 {
                    self.instance
                        .write_shared_rw_memory(&mut self.store, j, f_arr[j as usize])?;
                }
                self.instance
                    .set_input_signal(&mut self.store, msb, lsb, i as u32)?;
            }
        }

        let mut w = Vec::new();

        let witness_size = self.instance.get_witness_size(&mut self.store)?;
        for i in 0..witness_size {
            self.instance.get_witness(&mut self.store, i)?;
            let mut arr = vec![0; n32 as usize];
            for j in 0..n32 {
                arr[(n32 as usize) - 1 - (j as usize)] =
                    self.instance.read_shared_rw_memory(&mut self.store, j)?;
            }
            w.push(from_vec_u32(arr));
        }

        Ok(w)
    }

    // Implementation details for `calculate_witness` are omitted for brevity.
    /// Retrieves the witness buffer as a vector of bytes.
    /// Useful for serializing the witness data for transmission or storage.
    pub fn get_witness_buffer(&self, store: &mut impl AsStoreMut) -> Result<Vec<u8>> {
        let ptr = self.instance.get_ptr_witness_buffer(store)? as usize;
        let len = self.instance.get_n_vars(store)? * self.n64 * 8;
        let view = self.memory.view(store);
        let bytes = unsafe { view.data_unchecked() };

        let arr = bytes[ptr..ptr + len as usize].to_vec();

        Ok(arr)
    }
}

/// callback hooks for debugging
/// The `runtime` module provides callback hooks for interacting with the Wasm runtime.
/// These functions are used to handle specific runtime events like logging, errors, and memory inspection.
mod runtime {
    use super::AsStoreMut;
    use super::ExitCode;
    use super::Function;
    use super::RuntimeError;

    /// Creates a callback function for handling errors in the Wasm runtime.
    /// It logs the error details and triggers an early exit with a custom `ExitCode`.
    pub fn error(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        #[allow(clippy::many_single_char_names)]
        fn func(
            a: i32,
            b: i32,
            c: i32,
            d: i32,
            e: i32,
            f: i32,
        ) -> std::result::Result<(), RuntimeError> {
            // Details about the error are logged here.
            // The actual error handling logic can be adapted based on specific needs.
            println!("runtime error, exiting early: {a} {b} {c} {d} {e} {f}",);
            Err(RuntimeError::user(Box::new(ExitCode(1))))
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for handling exceptions in Circom 2.0 Wasm runtime.
    /// This is a stub function and can be expanded to include specific exception handling logic.
    pub fn exception_handler(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func(a: i32) {
            // Exception handling logic would be implemented here.
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for displaying shared read-write memory in Circom 2.0 Wasm runtime.
    /// This function can be used for debugging purposes to inspect memory states.
    pub fn show_memory(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func() {
            // Memory inspection logic would be implemented here.
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for printing error messages in the Circom 2.0 Wasm runtime.
    /// This can be used to output error messages for debugging and logging.
    pub fn print_error_message(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func() {
            // Error message printing logic would be implemented here.
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for writing buffer messages in the Circom 2.0 Wasm runtime.
    /// This can be used for logging and debugging purposes, especially for message passing within Wasm.
    pub fn write_buffer_message(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func() {
            // Buffer message writing logic would be implemented here.
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for logging signal-related events in the Wasm runtime.
    /// This is useful for tracking and debugging signal operations within the Wasm instance.
    pub fn log_signal(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func(a: i32, b: i32) {
            // Signal logging logic would be implemented here.
        }
        Function::new_typed(store, func)
    }

    /// Creates a callback for logging component-related events in the Wasm runtime.
    /// This function assists in debugging and understanding the component lifecycle within the Wasm instance.
    pub fn log_component(store: &mut impl AsStoreMut) -> Function {
        #[allow(unused)]
        fn func(a: i32) {
            // Component logging logic would be implemented here.
        }
        Function::new_typed(store, func)
    }
}
