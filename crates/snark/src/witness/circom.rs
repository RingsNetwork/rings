//! A module for interacting with Circom-generated WebAssembly (Wasm) in Rust.
//! Provides functionality to initialize, interact with, and extract data from Wasm instances,
//! specifically tailored for Circom, a tool for zero-knowledge proof circuits.
//! ==========
//! Copyright (c) 2021 Georgios Konstantopoulos
//! Copyright (c) Lurk Lab
//! Copyright (c) 2024 Rings Network
//! SPDX-License-Identifier: MIT
//!
//! Contributors:
//!
//! - Hanting Zhang (winston@lurk-lab.com)
//!   - Adapted the original work here: <https://!github.com/arkworks-rs/circom-compat/blob/master/src/witness/circom.rs>
//!
//! - Rings Dev
//!   - refine the original work here: <https://github.com/lurk-lab/circom-scotia/tree/main/src/witness>
use wasmer::AsStoreMut;
use wasmer::Function;
use wasmer::Instance;
use wasmer::Value;

use crate::error::Result;

/// Represents a Wasm runtime instance.
/// Encapsulates a `wasmer::Instance` to provide higher-level operations specific to Circom Wasm modules.
#[derive(Clone, Debug)]
pub struct Wasm(Instance);

/// Trait for read circom wasm
pub trait CircomBase {
    /// Initializes the Wasm instance with a given store.
    /// Performs an optional sanity check.
    fn init(&self, store: &mut impl AsStoreMut, sanity_check: bool) -> Result<()>;
    /// Retrieves a reference to a Wasm function by its name.
    fn func(&self, name: &str) -> &Function;
    /// Get wrapped wasm function
    fn get_ptr_witness_buffer(&self, store: &mut impl AsStoreMut) -> Result<u32>;
    /// Retrieves a pointer to a specific witness in the Wasm store.
    fn get_ptr_witness(&self, store: &mut impl AsStoreMut, w: u32) -> Result<u32>;
    /// Fetches the number of variables in the current instance.
    fn get_n_vars(&self, store: &mut impl AsStoreMut) -> Result<u32>;
    /// Retrieves the offset for a given signal.
    fn get_signal_offset32(
        &self,
        store: &mut impl AsStoreMut,
        p_sig_offset: u32,
        component: u32,
        hash_msb: u32,
        hash_lsb: u32,
    ) -> Result<()>;
    /// Sets an input signal in the Wasm instance.
    fn set_signal(
        &self,
        store: &mut impl AsStoreMut,
        c_idx: u32,
        component: u32,
        signal: u32,
        p_val: u32,
    ) -> Result<()>;
    /// Retrieves a value from the Wasm instance as a `u32`.
    fn get_u32(&self, store: &mut impl AsStoreMut, name: &str) -> Result<u32>;
    /// Fetches the version of Circom used by the Wasm instance.
    /// Only exists natively in Circom2, hardcoded for Circom
    fn get_version(&self, store: &mut impl AsStoreMut) -> Result<u32>;
}

/// Trait for Circom-specific functionality.
/// Provides methods to access properties of the arithmetic field used in Circom circuits.
pub trait Circom {
    /// Returns the length of the field representation.
    fn get_fr_len(&self, store: &mut impl AsStoreMut) -> Result<u32>;

    /// Gets a pointer to the raw prime field used by the circuit.
    fn get_ptr_raw_prime(&self, store: &mut impl AsStoreMut) -> Result<u32>;
}

/// Trait for Circom2-specific functionality.
/// Enhances the capabilities provided in `Circom` for the updated Circom2 framework,
/// including advanced memory management and signal processing.
pub trait Circom2 {
    /// Gets the length of the field number in 32-bit units.
    /// This is specific to the arithmetic field used in Circom2 circuits.
    fn get_field_num_len32(&self, store: &mut impl AsStoreMut) -> Result<u32>;

    /// Fetches the raw prime field value from the Wasm instance.
    /// This is a key component in the arithmetic of Circom2 circuits.
    fn get_raw_prime(&self, store: &mut impl AsStoreMut) -> Result<()>;

    /// Reads a value from the shared read-write memory at a given index.
    /// This is part of Circom2's enhanced memory management capabilities.
    fn read_shared_rw_memory(&self, store: &mut impl AsStoreMut, i: u32) -> Result<u32>;

    /// Writes a value to the shared read-write memory at a given index.
    /// Enables manipulation of the circuit's state within the Wasm instance.
    fn write_shared_rw_memory(&self, store: &mut impl AsStoreMut, i: u32, v: u32) -> Result<()>;

    /// Sets an input signal in the Wasm instance.
    /// Essential for initializing the circuit with specific inputs.
    fn set_input_signal(
        &self,
        store: &mut impl AsStoreMut,
        hmsb: u32,
        hlsb: u32,
        pos: u32,
    ) -> Result<()>;

    /// Fetches the witness data for a given index from the Wasm instance.
    /// Witnesses are crucial for verifying the correctness of the circuit's computation.
    fn get_witness(&self, store: &mut impl AsStoreMut, i: u32) -> Result<()>;

    /// Retrieves the size of the witness data.
    /// Useful for understanding the memory footprint and complexity of the circuit.
    fn get_witness_size(&self, store: &mut impl AsStoreMut) -> Result<u32>;
}

impl Circom for Wasm {
    fn get_fr_len(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getFrLen")
    }

    fn get_ptr_raw_prime(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getPRawPrime")
    }
}

impl Circom2 for Wasm {
    fn get_field_num_len32(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getFieldNumLen32")
    }

    fn get_raw_prime(&self, store: &mut impl AsStoreMut) -> Result<()> {
        let func = self.func("getRawPrime");
        func.call(store, &[])?;
        Ok(())
    }

    fn read_shared_rw_memory(&self, store: &mut impl AsStoreMut, i: u32) -> Result<u32> {
        let func = self.func("readSharedRWMemory");
        let result = func.call(store, &[i.into()])?;
        Ok(result[0].unwrap_i32() as u32)
    }

    fn write_shared_rw_memory(&self, store: &mut impl AsStoreMut, i: u32, v: u32) -> Result<()> {
        let func = self.func("writeSharedRWMemory");
        func.call(store, &[i.into(), v.into()])?;
        Ok(())
    }

    fn set_input_signal(
        &self,
        store: &mut impl AsStoreMut,
        hmsb: u32,
        hlsb: u32,
        pos: u32,
    ) -> Result<()> {
        let func = self.func("setInputSignal");
        func.call(store, &[hmsb.into(), hlsb.into(), pos.into()])?;
        Ok(())
    }

    fn get_witness(&self, store: &mut impl AsStoreMut, i: u32) -> Result<()> {
        let func = self.func("getWitness");
        func.call(store, &[i.into()])?;
        Ok(())
    }

    fn get_witness_size(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getWitnessSize")
    }
}

impl CircomBase for Wasm {
    fn init(&self, store: &mut impl AsStoreMut, sanity_check: bool) -> Result<()> {
        let func = self.func("init");
        func.call(store, &[Value::I32(i32::from(sanity_check))])?;
        Ok(())
    }

    fn get_ptr_witness_buffer(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getWitnessBuffer")
    }

    fn get_ptr_witness(&self, store: &mut impl AsStoreMut, w: u32) -> Result<u32> {
        let func = self.func("getPWitness");
        let res = func.call(store, &[w.into()])?;

        Ok(res[0].unwrap_i32() as u32)
    }

    fn get_n_vars(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        self.get_u32(store, "getNVars")
    }

    fn get_signal_offset32(
        &self,
        store: &mut impl AsStoreMut,
        p_sig_offset: u32,
        component: u32,
        hash_msb: u32,
        hash_lsb: u32,
    ) -> Result<()> {
        let func = self.func("getSignalOffset32");
        func.call(store, &[
            p_sig_offset.into(),
            component.into(),
            hash_msb.into(),
            hash_lsb.into(),
        ])?;

        Ok(())
    }

    fn set_signal(
        &self,
        store: &mut impl AsStoreMut,
        c_idx: u32,
        component: u32,
        signal: u32,
        p_val: u32,
    ) -> Result<()> {
        let func = self.func("setSignal");
        func.call(store, &[
            c_idx.into(),
            component.into(),
            signal.into(),
            p_val.into(),
        ])?;

        Ok(())
    }

    // Default to version 1 if it isn't explicitly defined
    fn get_version(&self, store: &mut impl AsStoreMut) -> Result<u32> {
        match self.0.exports.get_function("getVersion") {
            Ok(func) => Ok(func.call(store, &[])?[0].unwrap_i32() as u32), // should be 2
            Err(_) => Ok(1),
        }
    }

    fn get_u32(&self, store: &mut impl AsStoreMut, name: &str) -> Result<u32> {
        let func = self.func(name);
        let result = func.call(store, &[])?;
        Ok(result[0].unwrap_i32() as u32)
    }

    fn func(&self, name: &str) -> &Function {
        self.0
            .exports
            .get_function(name)
            .unwrap_or_else(|_| panic!("function {} not found", name))
    }
}

impl Wasm {
    /// Create a new wasm instance
    pub fn new(instance: Instance) -> Self {
        Self(instance)
    }
}
