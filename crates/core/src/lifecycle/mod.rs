//! Cooperative lifecycle primitives shared by native and browser runtimes.
//!
//! A [`StopSource`] is the authority that may request shutdown. A [`StopToken`]
//! is the read-only capability handed to long-running loops. The model is
//! intentionally monotonic: once a source requests stop, every token cloned from
//! that source observes stop forever. An `Epoch` notifies every waiter registered before an
//! event of its class, so a waiter that listens, checks its state predicate and only then
//! awaits is triggered by an event, never by a duration.

pub(crate) mod epoch;
mod stop;

pub use stop::StopSource;
pub use stop::StopToken;

#[cfg(test)]
mod test_lifecycle;
