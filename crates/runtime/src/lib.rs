#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]
#![doc = include_str!("../README.md")]

/// Apply the one cfg under which task scheduling exists: an executor is present — Tokio on
/// native (`tokio` feature) or the event loop in a browser.
macro_rules! with_executor {
    ($($item:item)*) => {
        $(
            #[cfg(any(feature = "tokio", all(feature = "browser", target_family = "wasm")))]
            $item
        )*
    };
}

mod bound;
#[cfg(all(feature = "browser", target_family = "wasm"))]
mod global;
mod timer;

with_executor! {
    mod task;

    pub use task::run_detached;
    pub use task::spawn_detached;
    pub use task::Abandoned;
    pub use task::DetachedError;
    pub use task::RuntimeUnavailable;
    pub use task::Spawner;
    pub use task::Unscheduled;
}

pub use bound::MaybeSend;
pub use bound::MaybeSendSync;
pub use timer::sleep;
pub use timer::TimerError;
pub use timer::UNBOUNDED_SLEEP;
