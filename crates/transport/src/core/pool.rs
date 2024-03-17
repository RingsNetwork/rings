//! A module implementing a generic round-robin pool for various transport systems.
//!
//! This module provides the foundation for creating a pool of resources (e.g., connections, channels) and
//! enables round-robin selection among these resources. It's designed with flexibility in mind, allowing
//! integration with different types of transport mechanisms. This ensures efficient and balanced resource
//! utilization across multiple channels or connections, irrespective of their specific implementation details.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;

use crate::error::Result;

/// Defines the behavior for managing resources in a round-robin manner.
///
/// This trait outlines essential operations for a round-robin resource pool, facilitating equitable
/// selection and access distribution among pooled resources. It's intended for various use cases where
/// managing a collection of elements (e.g., network connections, data channels) efficiently is crucial.
pub trait RoundRobin<T> {
    /// Selects a resource from the pool, ensuring it is in a ready state for use.
    fn select(&self) -> T;

    /// Provides a reference to all resources within the pool.
    fn all(&self) -> &Vec<T>;
}

/// Implements a round-robin pool for resources that can be cloned.
///
/// This structure provides a concrete round-robin pooling mechanism, supporting the sequential
/// selection of resources. It's generic over the resource type, requiring only that they implement
/// the `Clone` trait, thus ensuring wide applicability to various types of resources.
pub struct RoundRobinPool<T: Clone> {
    pool: Vec<T>,
    idx: AtomicUsize,
}

impl<T: Clone> RoundRobinPool<T> {
    /// Creates a new round-robin pool from a provided vector of resources.
    ///
    /// Initializes the pool with the specified resources and sets the initial selection index to zero.
    /// This is the entry point for creating a pool and managing resource selection in a round-robin fashion.
    pub fn from_vec(conns: Vec<T>) -> Self {
        Self {
            pool: conns,
            idx: AtomicUsize::from(0),
        }
    }
}

impl<T: Clone> RoundRobin<T> for RoundRobinPool<T> {
    /// Selects the next resource from the pool in a round-robin order.
    ///
    /// Safely increments the internal index to cycle through resources, ensuring each is selected
    /// sequentially. The method ensures thread-safety and atomicity in its operations, suitable for
    /// concurrent environments.
    fn select(&self) -> T {
        let len = self.pool.len();
        let idx = self
            .idx
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |x| {
                Some((x + 1) % len)
            })
            .expect("Unable to update index for round-robin selection.");

        self.pool[idx].clone()
    }

    /// Accesses all resources pooled, potentially for inspection or bulk operations.
    ///
    /// Offers direct access to the pool's underlying resources, enabling operations that require knowledge
    /// or manipulation of the entire collection of resources.
    fn all(&self) -> &Vec<T> {
        &self.pool
    }
}

/// A trait for pools capable of asynchronously sending messages through their resources.
///
/// Extends `RoundRobin` with functionality for asynchronous message transmission, leveraging the pooled
/// resources for communication. It's adaptable to various messaging patterns and data types, specified
/// by the generic `Message` associated type.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait MessageSenderPool<T>: RoundRobin<T> {
    /// The type of messages that can be sent through the pool.
    ///
    /// This associated type specifies the format and structure of messages suitable for transmission
    /// using the pool's resources. By defining this as a generic type, the trait allows for implementation
    /// with a wide variety of message types, making the pool versatile and adaptable to different
    /// communication needs and protocols.
    type Message;
    /// Asynchronously sends a message using one of the resources in the pool.
    ///
    /// A generic method accommodating different message types, facilitating their transmission
    /// through the pool's resources selected in a round-robin manner. It underscores the pool's
    /// versatility in handling diverse communication scenarios.
    async fn send(&self, msg: Self::Message) -> Result<()>;
}

/// A trait for assessing the readiness of all resources in a pool.
///
/// Enhances `RoundRobin` with the ability to verify the operational readiness of pooled resources.
/// It caters to use cases requiring assurance that all resources are prepared for task execution
/// or data handling before proceeding with operations.
pub trait StatusPool<T>: RoundRobin<T> {
    /// Evaluates the readiness of all pooled resources.
    ///
    /// Determines whether every resource in the pool is ready for operations, facilitating decision-making
    /// processes in resource management and task allocation.
    fn all_ready(&self) -> bool;
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_rr_pool() {
        let pool = RoundRobinPool::<usize>::from_vec(vec![1, 2, 3, 4]);
        assert_eq!(pool.select(), 1);
        assert_eq!(pool.select(), 2);
        assert_eq!(pool.select(), 3);
        assert_eq!(pool.select(), 4);
        assert_eq!(pool.select(), 1);
        assert_eq!(pool.select(), 2);
        assert_eq!(pool.select(), 3);
    }
}
