#![warn(missing_docs)]

//! Backend message types.
//!
//! The old closed `BackendMessage` enum and its `MessageHandler` dispatch have been
//! replaced by the namespaced [`Envelope`](crate::extension::ext::Envelope) wire and the
//! [`Extensions`](crate::extension::ext::Extensions) protocol registry. Only the SNARK
//! payload types remain here.

#[cfg(feature = "snark")]
pub mod snark;
