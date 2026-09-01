use serde::Deserialize;
use serde::Serialize;

/// Whole seconds since the Unix epoch supplied by a runtime adapter.
///
/// The type contains no clock access. It only gives the measurement state
/// relation an ordered, serializable time coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UnixTime(u64);

impl UnixTime {
    /// The Unix epoch.
    pub const EPOCH: Self = Self(0);

    /// Construct a timestamp from whole seconds since the Unix epoch.
    pub const fn from_secs(seconds: u64) -> Self {
        Self(seconds)
    }

    /// Return whole seconds since the Unix epoch.
    pub const fn as_secs(self) -> u64 {
        self.0
    }
}
