//! implementation of datachannel pool in for rings transport
//! ===============
use async_trait::async_trait;
use std::sync::atomic::Ordering;
use crate::core::transport::TransportMessage;
use std::sync::atomic::AtomicUsize;
use crate::error::Result;

/// Based trait of round robin pool
pub trait RoundRobin<T> {
    /// selected element should in ready status
    fn select(&self) -> T;
    /// Get all pooled elements
    fn all(&self) -> &Vec<T>;
}

/// Generic implementation of RoundRobinPool
pub struct RoundRobinPool<T: Clone> {
    pool: Vec<T>,
    idx: std::sync::atomic::AtomicUsize
}

impl <T: Clone> RoundRobinPool<T> {
    /// from a T list to pool
    pub fn from_vec(conns: Vec<T>) -> Self {
	Self {
	    pool: conns,
	    idx: AtomicUsize::from(0)
	}
    }
}

impl <T: Clone> RoundRobin<T> for RoundRobinPool<T> {
    /// select a T from pool
    fn select(&self) -> T {
	let len = self.pool.len();
	let idx = self.idx.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| Some((x + 1) % len)).expect("channel pool: cannot update index");
	let ret = self.pool[idx].clone();
	ret
    }

    /// get a ref of all polled element
    /// TODO: maybe optimize with parity_ready status
    fn all(&self) -> &Vec<T> {
	&self.pool
    }
}

/// Trait of channel pool, which defined send message operator
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait ChannelPool<T>: RoundRobin<T> {
    /// Send operator
    async fn send(&self, msg: TransportMessage) -> Result<()>;
}

/// Trait of status channel pool, which defined operator for checking all element is ready
pub trait ChannelPoolStatus<T>: RoundRobin<T> {
    /// Check all pooled element is ready
    fn all_ready(&self) -> bool;
}
