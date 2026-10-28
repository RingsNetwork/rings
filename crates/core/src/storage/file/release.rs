//! The release of a store, and its witness (#1085).
//!
//! Law (release): a store is released when its last holder drops it. Its holders are the
//! [`FileStorage`](super::FileStorage) handle and every operation still running on the blocking
//! pool, since each one holds the store until it ends (the execution law). Dropping the store
//! unlocks its directory (the ownership law, `lock`); only after that does its
//! [`StoreRelease`] resolve. The signal is the store's last field, and the fields of a struct
//! drop in declaration order, so the signal drops after the lock:
//!
//! ```text
//!   last holder drops ─▶ FileStore { …, lock, released } drops in field order
//!                          ─▶ lock unlocked ─▶ released dropped ─▶ every StoreRelease resolves
//! ```
//!
//! So once a [`StoreRelease`] resolves, an open of the store's directory in this process
//! succeeds. The witness is woken by the drop itself, never by time.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures::channel::oneshot;
use futures::future::Shared;
use futures::FutureExt;

/// The sending side of a store's release: held as the store's last field, never sent on, so
/// dropping it is the release (the release law).
pub(super) struct ReleaseSignal {
    /// Never sent on: its drop is the event.
    _dropped: oneshot::Sender<Infallible>,
}

/// Resolves once its store is released, its directory lock unlocked (the release law).
///
/// Cloning shares the witness: every clone resolves at the release.
#[derive(Clone)]
pub struct StoreRelease(Shared<oneshot::Receiver<Infallible>>);

/// A store's release signal, and the witness of its drop.
pub(super) fn release_signal() -> (ReleaseSignal, StoreRelease) {
    let (dropped, released) = oneshot::channel();
    (
        ReleaseSignal { _dropped: dropped },
        StoreRelease(released.shared()),
    )
}

impl Future for StoreRelease {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // The sender is never sent on: the only outcome is `Canceled`, the release.
        Pin::new(&mut self.0).poll(cx).map(|_| ())
    }
}

impl fmt::Debug for StoreRelease {
    /// The witness is opaque: whether it resolved is learned only by awaiting it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreRelease")
    }
}
