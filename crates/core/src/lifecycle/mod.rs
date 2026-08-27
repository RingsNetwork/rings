//! Cooperative lifecycle primitives shared by native and browser runtimes.
//!
//! A [`StopSource`] is the authority that may request shutdown. A [`StopToken`]
//! is the read-only capability handed to long-running loops. The model is
//! intentionally monotonic: once a source requests stop, every token cloned from
//! that source observes stop forever.

mod stop;

pub use stop::StopSource;
pub use stop::StopToken;

#[cfg(test)]
mod tests;
