use crate::prelude::rings_core::consts::*;

pub const BACKEND_MTU: usize = TRANSPORT_MAX_SIZE - TRANSPORT_MTU;
/// Redundant setting of vnode data storage
pub const DATA_REDUNDANT: u16 = 6;
/// Connect Behaviour
pub const CONNECT_FAILED_LIMIT: i16 = 3;
/// Message Send Behaviour
pub const MSG_SEND_FAILED_LIMIT: i16 = 10;
/// Message Received Behaviour
pub const MSG_RECV_FAILED_LIMIT: i16 = 10;
/// Timeout for proxied TCP connections
pub const TCP_SERVER_TIMEOUT: u64 = 30;
