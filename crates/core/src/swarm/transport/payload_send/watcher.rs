//! The node's admission watcher: one task that watches the first-frame admission of every
//! detached send, in place of one task per send (load-bounds paper, Obligation "Admission
//! complexity", #938).
//!
//! ```text
//!   send ─watch(w)─▶ channel ─▶ watcher: { w₁ … wₖ } polled independently ─▶ each settles
//!   first watch, or the watcher gone ─▶ start a watcher; no runtime ─▶ run w inline
//! ```
//!
//! Laws:
//! - **Independence.** The watcher polls every watch on its own waker, so a watch that waits
//!   out its deadline holds back no other: a send's admission is watched exactly as a task of
//!   its own would watch it.
//! - **Nothing abandoned.** A watch is handed to a running watcher, or to a new one, or run
//!   inline when no runtime can take a watcher, as `run_detached_or_inline` did per send: an
//!   abandoned watch would leave its transfer waiting for credit with no deadline. The watcher
//!   ends once the node drops its sending end and every watch it holds has settled.
//! - **Cost.** A send spawns no task; the node spawns one watcher, again only after one ended.

use std::future::poll_fn;
use std::sync::Mutex;
use std::task::Poll;

use futures::channel::mpsc;
use futures::stream::FuturesUnordered;
use futures::StreamExt;

use crate::swarm::detached::spawn_detached;
use crate::swarm::detached::DetachedTask;

/// The node's admission watcher (module documentation): the sending end of its running
/// watcher, if one runs.
#[derive(Default)]
pub(crate) struct AdmissionWatcher(Mutex<Option<mpsc::UnboundedSender<DetachedTask>>>);

impl AdmissionWatcher {
    /// Watch `watch` on the node's watcher, starting one if none runs, or run it inline when no
    /// runtime can take a watcher.
    pub(in crate::swarm::transport) async fn watch(&self, watch: DetachedTask) {
        let Some(watch) = self.hand_over(watch) else {
            return;
        };
        watch.await;
    }

    /// Hand `watch` to the running watcher, or to a new one; return it when no runtime can take
    /// a watcher.
    fn hand_over(&self, watch: DetachedTask) -> Option<DetachedTask> {
        let mut sender = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let watch = match sender.as_ref() {
            Some(running) => match running.unbounded_send(watch) {
                Ok(()) => return None,
                // The watcher ended: its receiver is gone, so the watch comes back.
                Err(error) => error.into_inner(),
            },
            None => watch,
        };
        let (started, receiver) = mpsc::unbounded();
        if spawn_detached(Box::pin(run_watcher(receiver))).is_err() {
            *sender = None;
            return Some(watch);
        }
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        rings_transport::hot_path::charge(rings_transport::hot_path::HotPath::ProtocolSend, 1);
        // A fresh receiver is open: the send cannot fail.
        let _ = started.unbounded_send(watch);
        *sender = Some(started);
        None
    }
}

/// Run a watcher over the watches `receiver` hands it, until the receiver is closed and every
/// watch settled.
async fn run_watcher(mut receiver: mpsc::UnboundedReceiver<DetachedTask>) {
    let mut watches = FuturesUnordered::new();
    let mut open = true;
    poll_fn(|context| {
        while open {
            match receiver.poll_next_unpin(context) {
                Poll::Ready(Some(watch)) => watches.push(watch),
                Poll::Ready(None) => open = false,
                Poll::Pending => break,
            }
        }
        while let Poll::Ready(Some(())) = watches.poll_next_unpin(context) {}
        match open || !watches.is_empty() {
            true => Poll::Pending,
            false => Poll::Ready(()),
        }
    })
    .await;
}
