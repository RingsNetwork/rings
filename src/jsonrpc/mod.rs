///! jsonrpc-server of rings-node
///! [JSON-RPC]: https://www.jsonrpc.org/specification
pub mod method;
pub mod response;
#[cfg(feature = "node")]
mod server;
#[cfg(feature = "node")]
pub use server::RpcMeta;

#[cfg(feature = "node")]
pub(crate) use self::server::build_handler;
