use std::sync::Mutex;
use std::sync::MutexGuard;

/// Acquire `mutex`, recovering the inner guard if a previous holder panicked.
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
