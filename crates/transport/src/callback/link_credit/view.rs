//! A read-only view of one lane's credit: its send side, for a sender that shapes its own
//! emission, and its receive side, for a receiver that judges whether its peer holds credit.
//!
//! ```text
//!   view(lane)       = (link, lane)            holds the link weakly: no connection kept alive
//!   free(view)       = limit − committed − reserved,   or None once the link is gone
//!   peer_unused(view) = advertised − received,         the peer's side of the same lane
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
#[cfg(rings_transport_backend)]
use std::sync::Arc;
use std::sync::Weak;
#[cfg(rings_transport_backend)]
use std::task::Poll;

#[cfg(rings_transport_backend)]
use super::CreditWait;
use super::LinkCredit;
use crate::core::credit::credit_index;
use crate::core::credit::CreditIndex;
use crate::core::pool::ChannelLane;

/// A read-only view of one lane's credit on one connection generation; see the module
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
    #[cfg(rings_transport_backend)]
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

    /// The frames the peer may send on the lane now: the credit this end advertised and the
    /// peer has not used (see `ReceiveWindow::unused`), or `None` once the connection
    /// generation is gone. A receiver judges by it whether a stalled lane is the peer's doing;
    /// it also counts credit in transit to the peer and frames in flight from it, at most one
    /// window in all with the frames this end holds, which a judge must allow for.
    pub fn peer_unused(&self) -> Option<u64> {
        let link = self.link.upgrade()?;
        let state = link.lock();
        (!state.closed).then(|| state.receive[self.index].unused())
    }

    /// Wait until the lane may send one frame without waiting for credit (`true`), or until the
    /// connection generation is gone (`false`). Reserves nothing (the read-only law).
    #[cfg(rings_transport_backend)]
    pub async fn credited(&self) -> bool {
        let mut wait = CreditWait::new(self.link.clone(), self.index);
        poll_fn(|context| {
            let Some(link) = self.link.upgrade() else {
                return Poll::Ready(false);
            };
            let mut state = link.lock();
            if state.closed {
                return Poll::Ready(false);
            }
            if state.send[self.index].free() > 0 {
                return Poll::Ready(true);
            }
            state.register_waiter(self.index, &mut wait.id, context.waker());
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

#[cfg(all(test, rings_transport_backend))]
mod test_view;
