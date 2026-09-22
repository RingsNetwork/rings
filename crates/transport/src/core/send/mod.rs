//! Shared send protocol: pure decisions, linear resources and an executor-neutral actor.
//!
//! Platform adapters supply the synchronous fence, command delivery and scheduling.
//! They do not implement alternative admission, failure or close state machines.

pub(crate) mod actor;
pub(crate) mod lifecycle;
pub(crate) mod model;
pub(crate) mod operation;
pub(crate) mod owner;

#[cfg(test)]
mod tests;
