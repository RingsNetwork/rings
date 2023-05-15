use crate::prelude::rings_core::consts::*;

pub const BACKEND_MTU: usize = TRANSPORT_MAX_SIZE - TRANSPORT_MTU;
/// Redundant setting of vnode data storage
pub const DATA_REDUNDANT: u16 = 1;
