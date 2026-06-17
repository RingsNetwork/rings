#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in protocol implements [`Extension`](crate::backend::ext::Extension)
//! and is registered under its namespace, replacing the corresponding
//! `BackendMessage` variant.

pub mod echo;

use crate::backend::ext::Extensions;

/// Register the built-in protocol extensions into a registry.
pub fn register_builtins(extensions: &Extensions) {
    extensions.register(echo::Echo);
}
