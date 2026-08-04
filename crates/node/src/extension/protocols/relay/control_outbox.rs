//! Bounded off-gate actor for relay terminal control frames.

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use rings_core::dht::Did;

use crate::extension::ext::Scope;

/// The queue bounds retained capabilities while its worker awaits an overlay send. Saturation is
/// an immediate effect error, never peer-controlled suspension of the protocol transition gate.
const MAX_PENDING_RELAY_CONTROL_SENDS: usize = 64;

struct ControlSend {
    scope: Scope,
    to: Did,
    payload: Bytes,
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

async fn apply_control_send(control: ControlSend) {
    #[cfg(all(
        test,
        feature = "node",
        not(all(feature = "browser", target_family = "wasm"))
    ))]
    if let Some(hook) = control.test_hook.as_ref() {
        hook.entered.notify_one();
        hook.release.notified().await;
    }
    if let Err(error) = control.scope.send(control.to, control.payload).await {
        tracing::debug!(peer = %control.to, ?error, "relay terminal control send failed");
    }
}

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
#[derive(Default)]
pub(crate) struct ControlSendTestHook {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(all(
    test,
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
impl ControlSendTestHook {
    pub(crate) async fn wait_until_blocked(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
#[derive(Default)]
pub(super) struct ControlOutbox {
    sender: Mutex<Option<tokio::sync::mpsc::Sender<ControlSend>>>,
    #[cfg(test)]
    test_hook: Option<Arc<ControlSendTestHook>>,
}

#[cfg(all(
    feature = "node",
    not(all(feature = "browser", target_family = "wasm"))
))]
impl ControlOutbox {
    #[cfg(test)]
    pub(super) fn with_test_hook(hook: Arc<ControlSendTestHook>) -> Self {
        Self {
            sender: Mutex::new(None),
            test_hook: Some(hook),
        }
    }

    pub(super) fn enqueue(
        &self,
        scope: Scope,
        to: Did,
        payload: Bytes,
    ) -> crate::error::Result<()> {
        let mut slot = lock_sender(&self.sender)?;
        let sender = slot.get_or_insert_with(|| {
            let (sender, mut receiver) =
                tokio::sync::mpsc::channel::<ControlSend>(MAX_PENDING_RELAY_CONTROL_SENDS);
            tokio::spawn(async move {
                while let Some(control) = receiver.recv().await {
                    apply_control_send(control).await;
                }
            });
            sender
        });
        sender
            .try_send(ControlSend {
                scope,
                to,
                payload,
                #[cfg(test)]
                test_hook: self.test_hook.clone(),
            })
            .map_err(|error| outbox_error(to, error))
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
#[derive(Default)]
pub(super) struct ControlOutbox {
    sender: Mutex<Option<futures::channel::mpsc::Sender<ControlSend>>>,
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl ControlOutbox {
    pub(super) fn enqueue(
        &self,
        scope: Scope,
        to: Did,
        payload: Bytes,
    ) -> crate::error::Result<()> {
        let mut slot = lock_sender(&self.sender)?;
        let sender = slot.get_or_insert_with(|| {
            let (sender, mut receiver) =
                futures::channel::mpsc::channel::<ControlSend>(MAX_PENDING_RELAY_CONTROL_SENDS);
            wasm_bindgen_futures::spawn_local(async move {
                use futures::StreamExt as _;

                while let Some(control) = receiver.next().await {
                    apply_control_send(control).await;
                }
            });
            sender
        });
        sender
            .try_send(ControlSend { scope, to, payload })
            .map_err(|error| outbox_error(to, error))
    }
}

fn lock_sender<T>(slot: &Mutex<T>) -> crate::error::Result<std::sync::MutexGuard<'_, T>> {
    slot.lock().map_err(|_| {
        crate::error::Error::ExtensionError("relay control outbox lock is poisoned".to_string())
    })
}

fn outbox_error(to: Did, error: impl std::fmt::Display) -> crate::error::Error {
    crate::error::Error::ExtensionError(format!(
        "relay terminal control outbox rejected frame for {to}: {error}"
    ))
}
