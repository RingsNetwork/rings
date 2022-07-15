///! jsonrpc-server of rings-node
///! [JSON-RPC]: https://www.jsonrpc.org/specification
pub mod method;
pub mod response;
#[cfg(feature = "client")]
mod server;
#[cfg(feature = "client")]
pub use server::RpcMeta;

#[cfg(feature = "client")]
pub(crate) use self::server::build_handler;
