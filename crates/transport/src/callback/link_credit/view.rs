//! A read-only view of one lane's send credit, for a sender that shapes its own emission.
//!
//! ```text
//!   view(lane)       = (link, lane)            holds the link weakly: no connection kept alive
//!   free(view)       = limit − committed − reserved,   or None once the link is gone
//!   credited(view)   resolves once free(view) ≥ 1 (true), or once the link is gone (false)
//! ```
//!
//! Laws:
//!
//! - **Read-only.** A view reserves, commits and cancels nothing: no transition of the credit
//!   algebra reads it, so a view never changes what any send may do. A sender that reads `free`
//!   and then sends may still wait in [`LinkCredit::reserve`], if another send took the credit in
//!   between; the view only lets it avoid queueing behind a full window.
//! - **No lost wakeup.** [`LaneCreditView::credited`] checks the credit and registers its waker
//!   under one lock, and every transition that frees a credit (a grant, a cancelled
//!   reservation) or ends the link wakes the waiters after it committed.
//! - **Lifetime.** The view holds the link weakly, and a pending wait removes its waker when it
//!   ends, granted or abandoned.

#[cfg(rings_transport_backend)]
use std::future::poll_fn;
use std::sync::Arc;
use std::sync::Weak;
#[cfg(rings_transport_backend)]
use std::task::Poll;

use super::LinkCredit;
#[cfg(rings_transport_backend)]
use super::SendWaiter;
use crate::core::credit::credit_index;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;

/// A read-only view of one lane's send credit on one connection generation; see the module
/// documentation.
///
/// Clone law: clones view the same lane of the same connection generation.
#[derive(Clone)]
pub struct LaneCreditView {
    /// The connection generation's credit, held weakly.
    link: Weak<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
}

impl LaneCreditView {
    /// The view of `lane` of `link`.
    pub(crate) fn new(link: &Arc<LinkCredit>, lane: ChannelLane) -> Self {
        Self {
            link: Arc::downgrade(link),
            index: credit_index(lane),
        }
    }

    /// The view of a connection that has no credit to grant: gone, or never able to send.
    pub fn closed() -> Self {
        Self {
            link: Weak::new(),
            index: credit_index(ChannelLane::PRIORITY),
        }
    }

    /// The frames the lane may send now without waiting for credit, or `None` once the
    /// connection generation is gone.
    pub fn free(&self) -> Option<u64> {
        let link = self.link.upgrade()?;
        let state = link.lock();
        (!state.closed).then(|| state.send[self.index].free())
    }

    /// Wait until the lane may send one frame without waiting for credit (`true`), or until the
    /// connection generation is gone (`false`). Reserves nothing (the read-only law).
    #[cfg(rings_transport_backend)]
    pub async fn credited(&self) -> bool {
        let mut wait = ViewWait {
            link: self.link.clone(),
            index: self.index,
            id: None,
        };
        poll_fn(|context| {
            let Some(link) = wait.link.upgrade() else {
                return Poll::Ready(false);
            };
            let mut state = link.lock();
            if state.closed {
                return Poll::Ready(false);
            }
            if state.send[wait.index].free() > 0 {
                return Poll::Ready(true);
            }
            let id = *wait.id.get_or_insert_with(|| {
                let id = state.next_waiter;
                state.next_waiter = id.wrapping_add(1);
                id
            });
            let waiters = &mut state.send_waiters[wait.index];
            match waiters.iter_mut().find(|waiter| waiter.id == id) {
                Some(waiter) => waiter.waker.clone_from(context.waker()),
                None => waiters.push(SendWaiter {
                    id,
                    waker: context.waker().clone(),
                }),
            }
            Poll::Pending
        })
        .await
    }

    /// Without a backend no connection grants credit, so the wait ends at once: the link is
    /// gone.
    #[cfg(not(rings_transport_backend))]
    pub async fn credited(&self) -> bool {
        false
    }
}

/// One pending [`LaneCreditView::credited`]: its waker is removed when it ends.
#[cfg(rings_transport_backend)]
struct ViewWait {
    /// The link waited on.
    link: Weak<LinkCredit>,
    /// The credit index of the lane.
    index: CreditIndex,
    /// The wait's identity, once it has registered.
    id: Option<u64>,
}

#[cfg(rings_transport_backend)]
impl Drop for ViewWait {
    fn drop(&mut self) {
        if let (Some(id), Some(link)) = (self.id, self.link.upgrade()) {
            link.forget_waiter(self.index, id);
        }
    }
}

#[cfg(all(test, rings_transport_backend))]
mod test_view;
