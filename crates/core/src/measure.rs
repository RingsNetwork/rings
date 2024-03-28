//! This module provide the `Measure` struct and its implementations.
//! It is used to assess the reliability of remote peers.
#![warn(missing_docs)]
use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [Measure].
#[cfg(not(feature = "wasm"))]
pub type MeasureImpl = Box<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(feature = "wasm")]
pub type MeasureImpl = Box<dyn BehaviourJudgement>;

/// The tag of counters in measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeasureCounter {
    /// The number of sent messages.
    Sent,
    /// The number of failed to sent messages.
    FailedToSend,
    /// The number of received messages.
    Received,
    /// The number of failed to receive messages.
    FailedToReceive,
    /// The number of connected.
    Connect,
    /// The number of disconnect.
    Disconnected,
}

/// `Measure` is used to assess the reliability of peers by counting their behaviour.
/// It currently count the number of sent and received messages in a given period (1 hour).
/// The method [Measure::incr] should be called in the proper places.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait Measure {
    /// `incr` increments the counter of the given peer.
    async fn incr(&self, did: Did, counter: MeasureCounter);
    /// `get_count` returns the counter of the given peer.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64;
}

/// `BehaviourJudgement` trait defines a method `good` for assessing whether a node behaves well.
/// Any structure implementing this trait should provide a way to measure the "goodness" of a node.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait BehaviourJudgement: Measure {
    /// This asynchronous method should return a boolean indicating whether the node identified by `did` is behaving well.
    async fn good(&self, did: Did) -> bool;
}

/// `ConnectBehaviour` trait offers a default implementation for the `good` method, providing a judgement
/// based on a node's behavior in establishing connections.
/// The "goodness" of a node is measured by comparing the connection and disconnection counts against a given threshold.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait ConnectBehaviour<const THRESHOLD: i64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory connection behavior.
    async fn good(&self, did: Did) -> bool {
        let conn = self.get_count(did, MeasureCounter::Connect).await as i64;
        let disconn = self.get_count(did, MeasureCounter::Disconnected).await as i64;
        tracing::debug!(
            "[ConnectBehaviour] in Threadhold: {:}, connect: {:}, disconn: {:}, delta: {:}",
            THRESHOLD,
            conn,
            disconn,
            conn - disconn
        );
        (conn - disconn) < THRESHOLD
    }
}

/// `MessageSendBehaviour` trait provides a default implementation for the `good` method, judging a node's
/// behavior based on its message sending capabilities.
/// The "goodness" of a node is measured by comparing the sent and failed-to-send counts against a given threshold.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageSendBehaviour<const THRESHOLD: i64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory message sending behavior.
    async fn good(&self, did: Did) -> bool {
        let failed = self.get_count(did, MeasureCounter::FailedToSend).await;
        (failed as i64) < THRESHOLD
    }
}

/// `MessageRecvBehaviour` trait provides a default implementation for the `good` method, assessing a node's
/// behavior based on its message receiving capabilities.
/// The "goodness" of a node is measured by comparing the received and failed-to-receive counts against a given threshold.
#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
pub trait MessageRecvBehaviour<const THRESHOLD: i64>: Measure {
    /// This asynchronous method returns a boolean indicating whether the node identified by `did` has a satisfactory message receiving behavior.
    async fn good(&self, did: Did) -> bool {
        let failed = self.get_count(did, MeasureCounter::FailedToReceive).await;
        (failed as i64) < THRESHOLD
    }
}
