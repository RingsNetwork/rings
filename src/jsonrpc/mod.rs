///! jsonrpc-server of rings-node
///! [JSON-RPC]: https://www.jsonrpc.org/specification
pub mod method;
pub mod response;
#[cfg(feature = "default")]
mod server;
#[cfg(feature = "default")]
pub use server::RpcMeta;

#[cfg(feature = "default")]
pub(crate) use self::server::build_handler;
