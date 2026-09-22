//! Synchronous generation admission capability for the native execution adapter.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use crate::sync_utils::lock_recover;

/// Shared generation identity. Cloning addresses the same gate, never creates another generation.
#[derive(Clone)]
pub(crate) struct AdmissionGate {
    /// Only this gate may issue a first-poll lease or commit retirement under its lock.
    retired: Arc<Mutex<bool>>,
}

/// Non-constructible proof that a generation is open and first polling excludes retirement.
pub(crate) struct FirstPollLease<'a> {
    /// Released before failure reporting, preventing recursive acquisition during fencing.
    _guard: MutexGuard<'a, bool>,
}

impl AdmissionGate {
    /// Create one open generation; its state can only advance to retired.
    pub(crate) fn new() -> Self {
        Self {
            retired: Arc::new(Mutex::new(false)),
        }
    }

    /// Issue a lease only while the generation remains open.
    pub(crate) fn enter(&self) -> Option<FirstPollLease<'_>> {
        let guard = lock_recover(&self.retired);
        match *guard {
            true => None,
            false => Some(FirstPollLease { _guard: guard }),
        }
    }

    /// Commit retirement and its logical-state effects under the same admission lock.
    pub(crate) fn retire(&self, publish: impl FnOnce()) {
        let mut retired = lock_recover(&self.retired);
        *retired = true;
        publish();
    }
}
