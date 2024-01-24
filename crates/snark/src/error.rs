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
    #[error("Error on witness compilling: {0}")]
    WitnessWasmRuntimeError(Box<wasmer::RuntimeError>),
    /// Error on create wasm instance
    #[error("Error on create wasm instance: {0}")]
    WitnessWasmInstanceError(Box<wasmer::InstantiationError>),
    /// Wasm runtime error
    #[error("Error on wasm runtime: {0}")]
    WitnessCompileError(Box<wasmer::CompileError>),
    /// Failed on load wasm module
    #[error("Error on load wasm module: {0}")]
    WitnessIoCompileError(Box<wasmer::IoCompileError>),
    /// Error on load r1cs
    #[error("Error on load r1cs: {0}")]
    LoadR1CS(String),
    /// Invalid data when reading header
    #[error("Invalid data: {0}")]
    InvalidDataWhenReadingR1CS(String),
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
