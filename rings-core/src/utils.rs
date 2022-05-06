use chrono::Utc;

pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}
