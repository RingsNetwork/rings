//! This module provide the `Measure` struct and its implementations.
//! It is used to assess the reliability of remote peers.
#![warn(missing_docs)]
use crate::dht::Did;

/// The tag of counters in measure.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MeasureCounter {
    /// The number of sent messages.
    Sent,
    /// The number of failed to sent messages.
    FailedToSend,
    /// The number of received messages.
    Received,
    /// The number of failed to receive messages.
    FailedToReceive,
}

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [incr] should be called in the proper places.
pub trait Measure {
    /// `incr` increments the counter of the given peer.
    fn incr(&self, did: Did, counter: MeasureCounter);
    /// `get_count` returns the counter of the given peer.
    fn get_count(&self, did: Did, counter: MeasureCounter) -> u64;
}
