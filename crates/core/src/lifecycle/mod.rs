//! Cooperative lifecycle primitives shared by native and browser runtimes.
//!
//! A [`StopSource`] is the authority that may request shutdown. A [`StopToken`]
//! is the read-only capability handed to long-running loops. The model is
//! intentionally monotonic: once a source requests stop, every token cloned from
//! that source observes stop forever. An `Epoch` is the monotone event count a waiter stamps
//! before a computation and awaits after it, so the wait is triggered by an event, never by a
//! duration.

pub(crate) mod epoch;
mod stop;

pub use stop::StopSource;
pub use stop::StopToken;

#[cfg(test)]
mod test_lifecycle;
