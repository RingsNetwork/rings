//! Error module of snark crate

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors collections in rings-snark
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Request error from reqwest
    #[error("Invalid http request: {0}")]
    HttpRequestError(#[from] reqwest::Error),
    /// Error on load witness at path
    #[error("Error on load witness calculator at path {0}")]
    WASMFailedToLoad(String),
    /// Error on loading witness from binary
    #[error("Failed to load witnesses: {0}")]
    WitnessFailedOnLoad(String),
    /// Error on compiling witness
    #[error("Error on witness compiling: {0}")]
    WitnessWasmRuntimeError(Box<wasmer::RuntimeError>),
    /// Error on create wasm instance
    #[error("Error on create wasm instance: {0}")]
    WitnessWasmInstanceError(Box<wasmer::InstantiationError>),
    /// Error on create wasm memory
    #[error("Error on create wasm memory: {0}")]
    WitnessWasmMemoryError(String),
    /// Wasm runtime error
    #[error("Error on wasm runtime: {0}")]
    WitnessCompileError(Box<wasmer::CompileError>),
    /// Required Wasm export is not available
    #[error("Wasm export not found: {0}")]
    WitnessMissingExport(String),
    /// Wasm export returned a value that does not match the Circom ABI
    #[error("Invalid wasm return from {function}: expected {expected}, got {actual}")]
    WitnessInvalidReturn {
        /// Wasm function name.
        function: String,
        /// Expected return shape.
        expected: &'static str,
        /// Actual return shape.
        actual: String,
    },
    /// Unsupported Circom compiler ABI version
    #[error("Unsupported Circom version: {0}")]
    WitnessUnsupportedCircomVersion(u32),
    /// Invalid number of 32-bit words for a 256-bit integer
    #[error("Invalid U256 word length: expected {expected}, got {actual}")]
    WitnessInvalidU256WordLength {
        /// Expected number of 32-bit words.
        expected: usize,
        /// Actual number of 32-bit words.
        actual: usize,
    },
    /// Failed on load wasm module
    #[error("Error on load wasm module: {0}")]
    WitnessIoCompileError(Box<wasmer::IoCompileError>),
    /// Error on load r1cs
    #[error("Error on load r1cs: {0}")]
    LoadR1CS(String),
    /// Invalid data when reading header
    #[error("Invalid data: {0}")]
    InvalidDataWhenReadingR1CS(String),
    /// R1CS variable counts do not describe one constant, public wires, and auxiliary wires.
    #[error(
        "Invalid R1CS shape: num_inputs={num_inputs}, num_aux={num_aux}, num_variables={num_variables}"
    )]
    InvalidR1CSShape {
        /// Number of public input wires, including the constant-one wire.
        num_inputs: usize,
        /// Number of auxiliary witness wires.
        num_aux: usize,
        /// Number of total wires declared by the R1CS.
        num_variables: usize,
    },
    /// R1CS public inputs and outputs cannot be split into equal recursive state vectors.
    #[error("Invalid R1CS public IO shape: num_inputs={num_inputs}")]
    InvalidR1CSPublicIoShape {
        /// Number of public input wires, including the constant-one wire.
        num_inputs: usize,
    },
    /// Circom public input and output counts are not equal for recursive use.
    #[error(
        "Invalid recursive R1CS public IO shape: public_inputs={public_inputs}, public_outputs={public_outputs}"
    )]
    InvalidR1CSRecursiveIoShape {
        /// Number of public input wires declared by Circom.
        public_inputs: usize,
        /// Number of public output wires declared by Circom.
        public_outputs: usize,
    },
    /// A constraint references a wire outside the declared R1CS variable range.
    #[error("Invalid R1CS variable index: index={index}, num_variables={num_variables}")]
    InvalidR1CSVariableIndex {
        /// Referenced wire index.
        index: usize,
        /// Number of total wires declared by the R1CS.
        num_variables: usize,
    },
    /// Witness length does not match the declared R1CS variable count.
    #[error("Invalid witness length: expected={expected}, actual={actual}")]
    InvalidWitnessLength {
        /// Expected witness length from the R1CS variable count.
        expected: usize,
        /// Actual witness length.
        actual: usize,
    },
    /// Io Error
    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),
    /// Error on call nova snark
    #[error("Error on nova snark: {0}")]
    NovaError(#[from] nova_snark::errors::NovaError),
}

impl From<wasmer::RuntimeError> for Error {
    fn from(e: wasmer::RuntimeError) -> Self {
        Self::WitnessWasmRuntimeError(Box::new(e))
    }
}

impl From<wasmer::InstantiationError> for Error {
    fn from(e: wasmer::InstantiationError) -> Self {
        Self::WitnessWasmInstanceError(Box::new(e))
    }
}

impl From<wasmer::CompileError> for Error {
    fn from(e: wasmer::CompileError) -> Self {
        Self::WitnessCompileError(Box::new(e))
    }
}

impl From<wasmer::IoCompileError> for Error {
    fn from(e: wasmer::IoCompileError) -> Self {
        Self::WitnessIoCompileError(Box::new(e))
    }
}
