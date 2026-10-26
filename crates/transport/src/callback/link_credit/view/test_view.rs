//! Laws of the read-only lane credit view: it reads what the sending end holds, reserves
//! nothing, and its wait loses no wakeup.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use futures::task::waker;
use futures::task::ArcWake;
use futures::FutureExt;

use crate::callback::link_credit::LaneCreditView;
use crate::callback::link_credit::LinkCredit;
use crate::callback::link_credit::NodeFramePool;
use crate::core::credit::LaneSignal;
use crate::core::credit::LANE_CREDIT_WINDOW;
use crate::core::pool::ChannelLane;

/// The lane every law is checked on: the Application lane's index.
const LANE: ChannelLane = ChannelLane::PRIORITY;

/// A waker that records whether it was woken.
#[derive(Default)]
struct Flag(AtomicBool);

impl ArcWake for Flag {
    fn wake_by_ref(flag: &Arc<Self>) {
        flag.0.store(true, Ordering::SeqCst);
    }
}

/// A fresh connection generation's credit.
fn link() -> Arc<LinkCredit> {
    Arc::new(LinkCredit::new(Arc::from("peer"), NodeFramePool::new()))
}

/// Read-only: the view reads the free credit of its own lane, reading and waiting reserve
/// nothing, and a reservation returned unsent is free again.
#[test]
fn test_the_view_reads_its_lane_and_reserves_nothing() {
    let link = link();
    let view = LaneCreditView::new(&link, LANE);
    assert_eq!(view.free(), Some(LANE_CREDIT_WINDOW));
    assert_eq!(view.credited().now_or_never(), Some(true));
    assert_eq!(view.free(), Some(LANE_CREDIT_WINDOW));

    let held = (0..LANE_CREDIT_WINDOW)
        .map(|_| {
            link.reserve(LANE)
                .now_or_never()
                .expect("a free credit is reserved at once")
                .expect("the link is open")
        })
        .collect::<Vec<_>>();
    assert_eq!(view.free(), Some(0));
    assert_eq!(
        LaneCreditView::new(&link, ChannelLane::new(2)).free(),
        Some(1),
        "another lane's credit is its own: a data lane's floor"
    );

    drop(held);
    assert_eq!(view.free(), Some(LANE_CREDIT_WINDOW));
}

/// No lost wakeup: a wait on an exhausted lane is pending, the grant that frees a credit wakes
/// it and it ends `true`; a wait when the link closes ends `false`, and a closed link has no
/// free credit.
#[test]
fn test_a_wait_ends_at_the_grant_or_at_the_close() {
    let link = link();
    let view = LaneCreditView::new(&link, LANE);
    let _held = (0..LANE_CREDIT_WINDOW)
        .map(|_| link.reserve(LANE).now_or_never())
        .collect::<Vec<_>>();
    let flag = Arc::new(Flag::default());
    let waker = waker(Arc::clone(&flag));
    let mut context = Context::from_waker(&waker);

    let mut granted = pin!(view.credited());
    assert_eq!(granted.as_mut().poll(&mut context), Poll::Pending);
    link.apply(LANE, LaneSignal::Credit {
        limit: LANE_CREDIT_WINDOW + 1,
        generation: 0,
    });
    assert!(
        flag.0.swap(false, Ordering::SeqCst),
        "the grant wakes the wait"
    );
    assert_eq!(granted.as_mut().poll(&mut context), Poll::Ready(true));

    let _more = link.reserve(LANE).now_or_never();
    let mut closed = pin!(view.credited());
    assert_eq!(closed.as_mut().poll(&mut context), Poll::Pending);
    link.close();
    assert!(flag.0.load(Ordering::SeqCst), "the close wakes the wait");
    assert_eq!(closed.as_mut().poll(&mut context), Poll::Ready(false));
    assert_eq!(view.free(), None);
}
