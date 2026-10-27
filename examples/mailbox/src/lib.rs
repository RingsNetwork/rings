#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

mod files;
pub mod gateway;
pub mod imap;
pub mod mailbox;
pub mod node;
pub mod outbound;
pub mod receive;
mod serve;
pub mod smtp;
pub mod store;
