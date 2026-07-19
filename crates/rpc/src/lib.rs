//! rings rpc library
#![deny(missing_docs)]

/// Error types used by the RPC client and handlers.
pub mod error;
/// JSON-RPC client implementation.
pub mod jsonrpc;
/// JSON-RPC method names.
pub mod method;
/// Re-exported dependencies used by public RPC types.
pub mod prelude;
/// JSON-RPC request and response DTOs.
pub mod protos;
/// Shared RPC transport settings.
pub mod types;
