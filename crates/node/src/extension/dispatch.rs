//! Per-namespace dispatch of inbound envelopes.
//!
//! ```text
//!   inbox reader ──submit(from, envelope)──▶ queue[namespace] ──worker──▶ Extensions::dispatch
//!                   (never waits; a full queue drops, counted)    (one task per namespace)
//! ```
//!
//! Laws:
//! - Each registered namespace's envelopes are dispatched in arrival order, one at a time.
//! - Namespaces are independent: a protocol that waits holds back only its own queue.
//! - Only registered namespaces get a queue, so the number of tasks is bounded by the registry;
//!   an envelope of an unregistered namespace is dropped at once.

use std::collections::HashMap;

use futures::channel::mpsc;
use futures::StreamExt;
use rings_core::dht::Did;

use crate::extension::ext::Envelope;
use crate::extension::ext::Extensions;

/// Envelopes one namespace may hold before further ones are dropped.
const NAMESPACE_QUEUE_CAPACITY: usize = 256;

/// The dispatch queue of every namespace that has received an envelope.
pub(super) struct NamespaceQueues {
    extensions: Extensions,
    queues: HashMap<String, mpsc::Sender<(Did, Envelope)>>,
    /// Envelopes dropped because their namespace's queue was full.
    dropped: u64,
}

impl NamespaceQueues {
    /// No queue yet, over `extensions`.
    pub(super) fn new(extensions: Extensions) -> Self {
        Self {
            extensions,
            queues: HashMap::new(),
            dropped: 0,
        }
    }

    /// Queue `envelope` from `from` for its namespace's worker, starting the worker on first
    /// use. Never waits.
    pub(super) fn submit(&mut self, from: Did, envelope: Envelope) {
        if !self.extensions.contains(&envelope.namespace) {
            tracing::debug!(namespace = %envelope.namespace, "no protocol registered, dropping");
            return;
        }
        let namespace = envelope.namespace.clone();
        let extensions = &self.extensions;
        let queue = self
            .queues
            .entry(namespace)
            .or_insert_with(|| spawn_worker(extensions.clone()));
        if let Err(error) = queue.try_send((from, envelope)) {
            self.dropped = self.dropped.saturating_add(1);
            tracing::warn!(
                full = error.is_full(),
                dropped = self.dropped,
                "namespace dispatch queue refused an envelope; dropped"
            );
        }
    }
}

/// Start the worker of one namespace: it dispatches its envelopes in order until its queue is
/// dropped. Without a runtime to run it, the queue is returned closed, so every envelope is
/// dropped and counted.
fn spawn_worker(extensions: Extensions) -> mpsc::Sender<(Did, Envelope)> {
    let (sender, mut receiver) = mpsc::channel::<(Did, Envelope)>(NAMESPACE_QUEUE_CAPACITY);
    let worker = async move {
        while let Some((from, envelope)) = receiver.next().await {
            if let Err(error) = extensions.dispatch(from, envelope).await {
                tracing::warn!(?error, "protocol dispatch failed");
            }
        }
    };
    if rings_runtime::spawn_detached(worker).is_err() {
        tracing::error!("no runtime to run a namespace dispatch worker");
    }
    sender
}
