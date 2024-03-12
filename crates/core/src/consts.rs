//! Constant variables.
///
/// default ttl in ms
pub const DEFAULT_TTL_MS: u64 = 600 * 1000;
pub const MAX_TTL_MS: u64 = DEFAULT_TTL_MS * 10;
pub const TS_OFFSET_TOLERANCE_MS: u128 = 3000;
pub const DEFAULT_SESSION_TTL_MS: u64 = 30 * 24 * 3600 * 1000;
/// 60k
pub const TRANSPORT_MTU: usize = 60000;
/// 60M
pub const TRANSPORT_MAX_SIZE: usize = TRANSPORT_MTU * 1000;
pub const VNODE_DATA_MAX_LEN: usize = 1024;
