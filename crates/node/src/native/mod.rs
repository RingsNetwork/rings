/// API authentication, per-listener authorization floors, origin policy, and token-file lifecycle.
pub mod api_auth;
// Documented by its own module docs; an outer doc here would make rustdoc resolve the
// module's intra-doc links in this scope instead of its own.
pub mod bootstrap;
/// Native command-line client helpers.
pub mod cli;
/// Native-node configuration file model and defaults.
pub mod config;
/// Native JSON-RPC endpoint server.
pub mod endpoint;
/// Foreground native TUN gateway supervision.
pub mod gateway;
