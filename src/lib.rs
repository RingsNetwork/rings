#![feature(async_closure)]
// temporary add this to fix tonic_build caused clipply warn. Maybe remove after tonic upgrade.
#![allow(clippy::return_self_not_must_use)]
pub mod cli;
pub mod error;
pub mod ethereum;
pub mod logger;
pub mod service;
