//! A jsonrpc-server of rings-node.
/// [JSON-RPC]: `<https://www.jsonrpc.org/specification>`
pub mod method;
pub mod response;
pub mod server;
/// RpcMeta basic info struct
pub use server::RpcMeta;

/// MetaIoHandler add methods from `super::methods::*` with RpcMeta
#[cfg(feature = "node")]
pub(crate) use self::server::build_handler;
