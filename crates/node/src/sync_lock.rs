use std::sync::Mutex;
use std::sync::MutexGuard;

use crate::error::Error;
use crate::error::Result;

/// Acquire one synchronous state lock through the crate's closed poison-error mapping.
pub(crate) fn lock<T>(state: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    state.lock().map_err(|_| Error::Lock)
}
