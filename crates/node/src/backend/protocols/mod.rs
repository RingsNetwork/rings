#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in protocol implements [`Extension`](crate::backend::ext::Extension)
//! and is registered under its namespace, replacing the corresponding
//! `BackendMessage` variant.

pub mod plaintext;
