#[cfg(not(target_family = "wasm"))]
use std::sync::atomic::AtomicUsize;
#[cfg(not(target_family = "wasm"))]
use std::sync::atomic::Ordering;
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;
#[cfg(not(target_family = "wasm"))]
use std::sync::Mutex;

#[cfg(not(target_family = "wasm"))]
use async_trait::async_trait;

use super::*;
#[cfg(not(target_family = "wasm"))]
use crate::core::callback::AdmittedInboundMessage;
#[cfg(not(target_family = "wasm"))]
use crate::core::callback::TransportCallback;
#[cfg(not(target_family = "wasm"))]
use crate::notifier::Notifier;

#[cfg(not(target_family = "wasm"))]
type AdmittedPayloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

#[cfg(not(target_family = "wasm"))]
struct RecordingCallback {
    admitted: AdmittedPayloads,
}

#[cfg(not(target_family = "wasm"))]
struct InvalidRecordingCallback {
    invalid: Arc<AtomicUsize>,
}

#[cfg(not(target_family = "wasm"))]
struct PendingInvalidCallback;

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for RecordingCallback {
    async fn on_admitted_message(
        &self,
        message: AdmittedInboundMessage<'_>,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (cid, payload, capacity) = message.into_parts();
        self.admitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((cid.to_owned(), payload.to_vec()));
        drop((payload, capacity));
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for InvalidRecordingCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.invalid.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
#[async_trait]
impl TransportCallback for PendingInvalidCallback {
    async fn on_invalid_inbound_frame(
        &self,
        _cid: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        std::future::pending().await
    }
}

#[cfg(not(target_family = "wasm"))]
mod admission;
mod capacity;
#[cfg(not(target_family = "wasm"))]
mod capacity_handoff;
#[cfg(not(target_family = "wasm"))]
mod invalid_report;
mod wire;
