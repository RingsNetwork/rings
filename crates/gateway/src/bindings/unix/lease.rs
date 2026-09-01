//! Unix helper lease identity.

use serde::Deserialize;
use serde::Serialize;

/// Opaque resource lease returned by the Unix configuration helper.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct UnixLeaseId(String);

impl UnixLeaseId {
    /// Construct a validated nonempty lease identifier.
    pub fn new(value: String) -> Option<Self> {
        (!value.trim().is_empty()).then_some(Self(value))
    }

    /// Return the wire identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
