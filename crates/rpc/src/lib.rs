//! rings rpc library
#![deny(missing_docs)]
#![deny(clippy::missing_docs_in_private_items)]
#![cfg_attr(test, allow(clippy::missing_docs_in_private_items))]

/// Error types used by the RPC client and handlers.
pub mod error;
/// JSON-RPC client implementation.
pub mod jsonrpc;
/// JSON-RPC method names and their authorization classes.
pub mod method;
/// Re-exported dependencies used by public RPC types.
pub mod prelude;
/// JSON-RPC request and response DTOs.
pub mod protos;
