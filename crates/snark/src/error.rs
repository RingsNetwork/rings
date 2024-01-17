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
    /// Calculate witness error
    #[error("Failed to calculate witness: {0}")]
    CalculateWitness(#[from] eyre::Report),
    /// Error on compiling witness
    #[error("Error on witness compilling")]
    WitnessCompileError(#[from] wasmer::CompileError),
    /// Error on call nova snark
    #[error("Error on nova snark")]
    NovaError(#[from] nova_snark::errors::NovaError),
}
