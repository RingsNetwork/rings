//! A jsonrpc-server of rings-node.
/// [JSON-RPC]: `<https://www.jsonrpc.org/specification>`
// pub mod method;
//pub mod response;
pub mod server;

/// MetaIoHandler add methods from `super::methods::*` with RpcMeta
pub mod handler;
