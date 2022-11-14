#![warn(missing_docs)]
//! A peer is a remote node.
//! This module provides data structures for keeping information about a peer.
use serde::Deserialize;
use serde::Serialize;

/// Used to describe what services peer offers
#[derive(Debug, Serialize, Deserialize, PartialEq, PartialOrd, Eq, Ord, Hash, Clone)]
pub enum PeerService {
    /// Services defined externally
    Custom(String),
}
