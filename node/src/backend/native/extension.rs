#![warn(missing_docs)]

//! This module supports a user-defined message handler based on WebAssembly (Wasm).
//! The Rings network allows loading a Wasm or Wat file from a remote or local file and transforming it into a message handler.
//! The Wasm module should satisfy the following requirements:
//!
//! 1. It should have a function with the signature fn handler(param: ExternRef) -> ExternRef, and this function should be exported.
//!
//! 2. The Wasm module should not have any external imports, except for the helper functions defined by the Rings network.
//!
//! 3. Only the helper functions defined by the Rings network can be used, which include:
//!
//! ```text
//!     "message_abi" => {
//!         "message_type"  => msg_type,
//!         "extra" => extra,
//!         "data" => data,
//!         "read_at" => read_at,
//!         "write_at" => write_at
//!     }
//! ```
//! A basic wasm extension may looks like:
//!
//! ```wat
//! (module
//!  ;; Let's import message_type from message_abi
//!  (type $ty_message_type (func (param externref) (result i32)))
//!  (import "message_abi" "message_type" (func $message_type (type $ty_message_type)))
//!   ;; fn handler(param: ExternRef) -> ExternRef
//!  (func $handler  (param externref) (result externref)
//!      (return (local.get 0))
//!  )
//!  (export "handler" (func $handler))
//! )
//!
//! You can see that this wat/wasm extension defines a handler function and
//! imports the message_type ABI.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use rings_core::message::MessagePayload;
use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::processor::Processor;
use crate::backend::types::MessageEndpoint;

/// Path of a wasm extension
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// Local filesystem path
    Local(String),
    /// A remote resource needs to fetch
    Remote(String),
}

/// Configure for Extension
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionConfig {
    /// Path of extension, can be remote or local
    pub paths: Vec<Path>,
}

/// Manager of Extension
pub struct Extension {
    #[allow(dead_code)]
    processor: Arc<Processor>,
}

/// Calls the extension handler with the given message and returns the response.
pub trait ExtensionHandlerCaller {
    /// Call extension handler
    fn call(&self, msg: Bytes) -> Result<()>;
}

impl Extension {
    /// Creates a new Extension instance with the specified configuration.
    pub async fn new(_config: &ExtensionConfig, processor: Arc<Processor>) -> Result<Self> {
        Ok(Self { processor })
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MessageEndpoint<Bytes> for Extension {
    /// Handles the incoming message by passing it to the extension handlers.
    async fn handle_message(&self, _ctx: &MessagePayload, _data: &Bytes) -> Result<()> {
        Ok(())
    }
}
