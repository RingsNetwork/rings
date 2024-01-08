//! Error module of snark crate

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Errors collections in rings-snark
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Request error from reqwest
    #[error("Invalid http request: {0}")]
    HttpRequestError(#[from] reqwest::Error),
    /// Error on load witness at path
    #[error("Error on load witness calculator at path {0}")]
    WASMFailedToLoad(String)
}
