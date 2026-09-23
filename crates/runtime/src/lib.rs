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

mod bound;
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub mod global;
mod task;
mod timer;

pub use bound::MaybeSend;
pub use bound::MaybeSendSync;
pub use task::run_detached;
pub use task::spawn_detached;
pub use task::Abandoned;
pub use task::DetachedError;
pub use task::RuntimeUnavailable;
pub use task::Spawner;
pub use task::Unscheduled;
pub use timer::sleep;
pub use timer::TimerError;
pub use timer::UNBOUNDED_SLEEP;
