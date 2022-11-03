//! Utils for ring-core
use chrono::Utc;

/// Get local utc timestamp (millsecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}
