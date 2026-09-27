//! Per-link credit flow control (#904): the receiver of one link generation paces its sender at
//! the rate it releases frames, so the transport never tail-drops a frame of an honest sender.
//!
//! ```text
//! sender (per link generation):    sent, acked                 in_flight = sent − acked
//!   payload frame:  wait until in_flight < w;  claim (sent += 1);  send
//!                   irrevocable or accepted ⇒ commit;  otherwise ⇒ refund (sent −= 1)
//!   Credit(r):      acked ← max(acked, min(r, sent));  wake the waiters
//! receiver (per link generation):  released, returned
//!   payload frame released (processed, dropped on any path, or refused by the transport)
//!                   released += 1;  released − returned ≥ w/2 ⇒ return
//!   return:         one returner at a time:  send Credit(released) on the link, awaited;
//!                   delivered ⇒ returned ← max(returned, released);
//!                   refused locally, or the node congested ⇒ retry after LINK_CREDIT_RETRY
//! ```
//!
//! A payload frame is *released* once it holds no receiver budget at all: the transport's
//! inbound permit and core's inbound capacity both, so after the inbound actor has processed it,
//! or at whichever drop ends it earlier. Link-control frames, the credit returns included, are
//! counted by neither end.
//!
//! Laws (tested in `tests` below and, end to end, in `tests::default::test_link_credit`):
//!
//! - **No transport drop.** A sender has at most `w` payload frames unreleased at its receiver
//!   (`in_flight ≤ w`, since `acked` counts only released frames), and
//!   `w = INBOUND_PEER_FRAME_CAPACITY / 2`, `w · MAX_DATA_CHANNEL_MESSAGE_SIZE ≤
//!   INBOUND_PEER_BYTE_CAPACITY / 2`, so an honest sender never exceeds the transport's
//!   per-peer bound, and half of it stays free for link control.
//! - **Budget.** Credit only ever delays a frame, never adds one, so every rate bound a sender
//!   keeps without credit (the onion emitter's L9 budget) still holds.
//! - **Liveness.** A stalled sender has `in_flight = w`; once its receiver releases those frames,
//!   `released − returned ≥ w ≥ w/2`, so a return is due and is retried until it is delivered.
//!   The count is cumulative, so a lost or repeated return is repaired by the next.
//!   The law's premise is that every payload frame is eventually released: the channel is
//!   ordered and reliable, the token of an arrived frame releases it on every path that ends
//!   it, and the transport reports every frame it refuses. A frame that were never released
//!   would, with a lost return, leave `released − returned` below `w/2` for good and stall the
//!   link; the premise rules that out.
//! - **Generation.** Every generation starts with `w` credits and counts from zero; a return is
//!   applied only to the generation it was sent on, so no credit crosses generations.
//! - **Volume hiding.** The credit a sender holds depends only on how fast its receiver releases
//!   frames, never on what the frames carry.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use futures::channel::oneshot;
use rings_transport::callback::INBOUND_PEER_BYTE_CAPACITY;
use rings_transport::callback::INBOUND_PEER_FRAME_CAPACITY;
use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

use super::PendingConnectionAttempt;
use super::SwarmTransport;
use crate::swarm::detached::spawn_detached;

/// `w`: the payload frames one link generation may have unreleased at its receiver.
pub const LINK_CREDIT_WINDOW: u64 = (INBOUND_PEER_FRAME_CAPACITY / 2) as u64;

/// `w/2`: a receiver returns its count once this many frames were released since its last
/// delivered return.
pub(crate) const LINK_CREDIT_RETURN_BATCH: u64 = LINK_CREDIT_WINDOW / 2;

/// How long a receiver waits before it retries a return its link refused (the link-control
/// budget spent, no runtime), or delays one while its node is congested.
#[cfg(not(test))]
const LINK_CREDIT_RETRY: Duration = Duration::from_millis(250);
/// See the production value.
#[cfg(test)]
const LINK_CREDIT_RETRY: Duration = Duration::from_millis(10);

// No transport drop: `w` frames of the widest size fit half of the per-peer byte bound, and
// `w` itself is half of the per-peer frame bound.
const _: () = assert!(
    (LINK_CREDIT_WINDOW as usize) * MAX_DATA_CHANNEL_MESSAGE_SIZE <= INBOUND_PEER_BYTE_CAPACITY / 2
        && 2 * (LINK_CREDIT_WINDOW as usize) <= INBOUND_PEER_FRAME_CAPACITY
        && LINK_CREDIT_RETURN_BATCH > 0
);

/// Whether a sender with `sent` frames of which `acked` are released may send one more.
pub(crate) const fn may_send(sent: u64, acked: u64) -> bool {
    sent.saturating_sub(acked) < LINK_CREDIT_WINDOW
}

/// Whether a receiver that has released `released` frames and delivered `returned` owes its
/// sender a return.
pub(crate) const fn return_due(released: u64, returned: u64) -> bool {
    released.saturating_sub(returned) >= LINK_CREDIT_RETURN_BATCH
}

/// The outcome of one credit return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditReturn {
    /// The return was handed to the link's ordered, reliable data channel.
    Delivered,
    /// The link refused it for now (its link-control budget spent, no runtime, a send that did
    /// not complete): the returner retries.
    Retry,
    /// The generation is retired: nothing is returned any more.
    Gone,
}

/// The sender's ledger of one link generation.
pub(crate) struct SendCredit {
    /// The generation the ledger counts.
    generation: u64,
    /// The counts and the waiters.
    state: Mutex<SendCreditState>,
}

/// The mutable part of a [`SendCredit`].
#[derive(Default)]
struct SendCreditState {
    /// Payload frames claimed and not refunded.
    sent: u64,
    /// The largest release count returned, never above `sent`.
    acked: u64,
    /// The waiters for the next change, woken all at once.
    waiters: Vec<oneshot::Sender<()>>,
}

impl SendCreditState {
    /// Wake every waiter: the credit changed.
    fn wake(&mut self) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(());
        }
    }
}

impl SendCredit {
    /// The ledger of a fresh `generation`: nothing sent, `w` credits.
    fn new(generation: u64) -> Self {
        Self {
            generation,
            state: Mutex::new(SendCreditState::default()),
        }
    }

    /// The counts under the lock, recovered if poisoned: they stay consistent under every
    /// operation, which updates them in one step.
    fn state(&self) -> std::sync::MutexGuard<'_, SendCreditState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether one more frame may be sent now.
    pub(crate) fn is_available(&self) -> bool {
        let state = self.state();
        may_send(state.sent, state.acked)
    }

    /// The frames sent and not yet released.
    pub(crate) fn in_flight(&self) -> u64 {
        let state = self.state();
        state.sent.saturating_sub(state.acked)
    }

    /// Claim one credit for a frame about to be sent, if the window allows it; the claim is
    /// refunded unless it is committed.
    pub(crate) fn try_claim(self: &Arc<Self>) -> Option<CreditClaim> {
        let mut state = self.state();
        if !may_send(state.sent, state.acked) {
            return None;
        }
        state.sent = state.sent.saturating_add(1);
        Some(CreditClaim {
            credit: Arc::clone(self),
            committed: false,
        })
    }

    /// A wake-up for the next change of the credit, fired at once if a frame may be sent now.
    /// It is registered under the same lock as the check, so no change is missed.
    pub(crate) fn changed(&self) -> oneshot::Receiver<()> {
        let (waiter, woken) = oneshot::channel();
        let mut state = self.state();
        if may_send(state.sent, state.acked) {
            let _ = waiter.send(());
        } else {
            state.waiters.push(waiter);
        }
        woken
    }

    /// The receiver has released `released` frames of this generation: advance `acked`, never
    /// past what was sent and never backwards.
    pub(crate) fn acknowledge(&self, released: u64) {
        let mut state = self.state();
        let acked = released.min(state.sent).max(state.acked);
        if acked != state.acked {
            state.acked = acked;
            state.wake();
        }
    }

    /// A claimed frame was never sent: return its credit.
    fn refund(&self) {
        let mut state = self.state();
        state.sent = state.sent.saturating_sub(1).max(state.acked);
        state.wake();
    }
}

/// One credit claimed for a frame about to be sent: refunded on drop unless committed.
pub(crate) struct CreditClaim {
    /// The ledger the credit was claimed from.
    credit: Arc<SendCredit>,
    /// Whether the frame left.
    committed: bool,
}

impl CreditClaim {
    /// The frame left: its credit stays spent until the receiver releases the frame.
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for CreditClaim {
    fn drop(&mut self) {
        if !self.committed {
            self.credit.refund();
        }
    }
}

/// The ledger of one peer's current link generation, as the sending end keeps it across its
/// outbound workers. Clone law: clones name the same ledger.
#[derive(Clone, Default)]
pub(crate) struct LinkCredits(Arc<Mutex<Option<Arc<SendCredit>>>>);

impl LinkCredits {
    /// The slot under the lock, recovered if poisoned: it holds one pointer.
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Arc<SendCredit>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ledger frames of `generation` are sent under: a newer generation than the current
    /// one replaces it with a fresh ledger of `w` credits (Law Generation), and an older one has
    /// none, since its link is retired.
    pub(crate) fn for_sending(&self, generation: u64) -> Option<Arc<SendCredit>> {
        let mut slot = self.slot();
        match slot.as_ref() {
            Some(current) if current.generation == generation => Some(Arc::clone(current)),
            Some(current) if current.generation > generation => None,
            _ => {
                let fresh = Arc::new(SendCredit::new(generation));
                *slot = Some(Arc::clone(&fresh));
                Some(fresh)
            }
        }
    }

    /// The ledger of exactly `generation`, if frames were sent under it: where a return of that
    /// generation applies.
    pub(crate) fn of_generation(&self, generation: u64) -> Option<Arc<SendCredit>> {
        self.slot()
            .as_ref()
            .filter(|current| current.generation == generation)
            .cloned()
    }
}

/// A read-only view of the credit of one link generation, for a sender that paces itself on it
/// (the onion link emitter): whether a frame may be sent now, and a wake-up for when that
/// changes. A link with no ledger yet has sent nothing, so its whole window is available.
#[derive(Clone)]
pub struct LinkCredit(Option<Arc<SendCredit>>);

impl LinkCredit {
    /// The view of `credit`, or of an unused window.
    pub(crate) const fn new(credit: Option<Arc<SendCredit>>) -> Self {
        Self(credit)
    }

    /// Whether a frame may be sent on the link now.
    pub fn is_available(&self) -> bool {
        self.0.as_ref().is_none_or(|credit| credit.is_available())
    }

    /// The frames sent on the link and not yet released by its receiver.
    pub fn in_flight(&self) -> u64 {
        self.0.as_ref().map_or(0, |credit| credit.in_flight())
    }

    /// Resolve once a frame may be sent on the link, at once if one may now; registered under
    /// the ledger's lock, so no release is missed.
    pub async fn available(&self) {
        if let Some(credit) = self.0.as_ref() {
            let _ = credit.changed().await;
        }
    }
}

/// The receiver's count of one link generation: the payload frames released, and the count its
/// sender has been told of.
pub(crate) struct ReleaseLedger {
    /// The generation the ledger counts.
    attempt: PendingConnectionAttempt,
    /// The transport the returns leave through.
    transport: Weak<SwarmTransport>,
    /// Payload frames released.
    released: AtomicU64,
    /// The largest count delivered to the sender.
    returned: AtomicU64,
    /// Whether a returner runs: at most one does.
    returning: AtomicBool,
}

impl ReleaseLedger {
    /// The ledger of `attempt`'s generation, returning through `transport`.
    pub(crate) fn new(attempt: PendingConnectionAttempt, transport: &Arc<SwarmTransport>) -> Self {
        Self {
            attempt,
            transport: Arc::downgrade(transport),
            released: AtomicU64::new(0),
            returned: AtomicU64::new(0),
            returning: AtomicBool::new(false),
        }
    }

    /// The token of one arrived payload frame, which releases it when dropped.
    pub(crate) fn token(self: &Arc<Self>) -> ReleaseToken {
        ReleaseToken(Arc::clone(self))
    }

    /// One payload frame was released: count it, and start the returner if a return is due and
    /// none runs.
    pub(crate) fn release(self: &Arc<Self>) {
        self.released.fetch_add(1, Ordering::AcqRel);
        self.start_return();
    }

    /// Start the one returner if a return is due and none runs; a runtime that cannot carry it
    /// leaves the return to the next release.
    fn start_return(self: &Arc<Self>) {
        if !self.is_due() || self.returning.swap(true, Ordering::AcqRel) {
            return;
        }
        if spawn_detached(Box::pin(return_credits(Arc::clone(self)))).is_err() {
            self.returning.store(false, Ordering::Release);
        }
    }

    /// Whether a return is due now.
    fn is_due(&self) -> bool {
        return_due(
            self.released.load(Ordering::Acquire),
            self.returned.load(Ordering::Acquire),
        )
    }

    /// The returner stops: clear the flag, and keep running if a release raced the check.
    /// Post: `true` iff this returner still owns the flag and a return is due.
    fn resume_after_stop(&self) -> bool {
        self.returning.store(false, Ordering::Release);
        self.is_due() && !self.returning.swap(true, Ordering::AcqRel)
    }
}

/// One arrived payload frame of a link generation: released when dropped, whichever path ends
/// the frame (processed, held and dropped, refused, or failed).
pub(crate) struct ReleaseToken(Arc<ReleaseLedger>);

impl Drop for ReleaseToken {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// The returner of one ledger (see the module diagram): it returns the cumulative count until
/// no return is due, retrying a return the link refused, and delaying while the node is
/// congested. It ends for good once its generation is gone.
async fn return_credits(ledger: Arc<ReleaseLedger>) {
    loop {
        if !ledger.is_due() {
            if ledger.resume_after_stop() {
                continue;
            }
            return;
        }
        let Some(transport) = ledger.transport.upgrade() else {
            return;
        };
        if transport.is_inbound_congested() {
            drop(transport);
            let _ = rings_runtime::sleep(LINK_CREDIT_RETRY).await;
            continue;
        }
        let released = ledger.released.load(Ordering::Acquire);
        match transport.return_link_credit(ledger.attempt, released).await {
            CreditReturn::Delivered => {
                ledger.returned.fetch_max(released, Ordering::AcqRel);
            }
            CreditReturn::Gone => return,
            CreditReturn::Retry => {
                drop(transport);
                let _ = rings_runtime::sleep(LINK_CREDIT_RETRY).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::may_send;
    use super::return_due;
    use super::LinkCredit;
    use super::LinkCredits;
    use super::SendCredit;
    use super::LINK_CREDIT_RETURN_BATCH;
    use super::LINK_CREDIT_WINDOW;

    /// The pure rules: a sender sends while fewer than `w` frames are unreleased, and a receiver
    /// returns once `w/2` frames were released since its last delivered return.
    #[test]
    fn test_the_window_and_the_return_batch() {
        assert!(may_send(LINK_CREDIT_WINDOW - 1, 0));
        assert!(!may_send(LINK_CREDIT_WINDOW, 0));
        assert!(may_send(LINK_CREDIT_WINDOW, 1));
        assert!(!return_due(LINK_CREDIT_RETURN_BATCH - 1, 0));
        assert!(return_due(LINK_CREDIT_RETURN_BATCH, 0));
        assert!(!return_due(3, 5), "a return never runs ahead of the count");
    }

    /// No transport drop: a ledger admits exactly `w` claims, and a claim dropped uncommitted
    /// (its frame never left) gives its credit back.
    #[test]
    fn test_a_ledger_admits_one_window_and_refunds_what_never_left() {
        let credit = Arc::new(SendCredit::new(1));
        let claims = (0..LINK_CREDIT_WINDOW)
            .map(|_| credit.try_claim().expect("within the window"))
            .collect::<Vec<_>>();
        assert!(credit.try_claim().is_none());
        assert_eq!(credit.in_flight(), LINK_CREDIT_WINDOW);

        let mut claims = claims.into_iter();
        drop(claims.next());
        assert_eq!(
            credit.in_flight(),
            LINK_CREDIT_WINDOW - 1,
            "an unsent frame refunds"
        );
        claims.for_each(|claim| claim.commit());
        assert!(credit.try_claim().is_some());
    }

    /// Liveness: returns are cumulative, so a stale, repeated or overstated return changes
    /// nothing wrong: `acked` only grows, and never past what was sent.
    #[test]
    fn test_returns_are_monotone_and_bounded_by_what_was_sent() {
        let credit = Arc::new(SendCredit::new(1));
        for _ in 0..LINK_CREDIT_WINDOW {
            credit.try_claim().expect("within the window").commit();
        }
        credit.acknowledge(20);
        assert_eq!(credit.in_flight(), LINK_CREDIT_WINDOW - 20);
        credit.acknowledge(16);
        assert_eq!(
            credit.in_flight(),
            LINK_CREDIT_WINDOW - 20,
            "a stale return is ignored"
        );
        credit.acknowledge(20);
        assert_eq!(
            credit.in_flight(),
            LINK_CREDIT_WINDOW - 20,
            "a repeated one is idempotent"
        );
        credit.acknowledge(u64::MAX);
        assert_eq!(
            credit.in_flight(),
            0,
            "an overstated return releases only what was sent"
        );
        assert!(credit.try_claim().is_some());
    }

    /// Generation: a newer generation starts a fresh ledger of `w` credits, whatever the older
    /// one had in flight; an older generation has no ledger, and a view of an unused link has
    /// its whole window.
    #[test]
    fn test_a_new_generation_resets_the_credit() {
        let credits = LinkCredits::default();
        let first = credits.for_sending(1).expect("a fresh ledger");
        for _ in 0..LINK_CREDIT_WINDOW {
            first.try_claim().expect("within the window").commit();
        }
        assert!(!first.is_available());

        let second = credits.for_sending(2).expect("a newer generation");
        assert!(second.is_available());
        assert_eq!(second.in_flight(), 0);
        assert!(
            credits.for_sending(1).is_none(),
            "a retired generation sends nothing"
        );
        assert!(
            credits.of_generation(1).is_none(),
            "its returns apply to nothing"
        );
        assert!(LinkCredit::new(None).is_available());
    }

    /// A waiter registered while the window is full is woken by the return that opens it.
    #[test]
    fn test_a_return_wakes_the_waiters() {
        let credit = Arc::new(SendCredit::new(1));
        for _ in 0..LINK_CREDIT_WINDOW {
            credit.try_claim().expect("within the window").commit();
        }
        let mut woken = credit.changed();
        assert_eq!(woken.try_recv(), Ok(None));
        credit.acknowledge(1);
        assert_eq!(woken.try_recv(), Ok(Some(())));
        let mut at_once = credit.changed();
        assert_eq!(
            at_once.try_recv(),
            Ok(Some(())),
            "an open window fires at once"
        );
    }
}
