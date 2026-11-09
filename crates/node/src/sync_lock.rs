use std::sync::Mutex;
use std::sync::MutexGuard;

use crate::error::Error;
use crate::error::Result;

/// Acquire one synchronous state lock through the crate's closed poison-error mapping.
pub(crate) fn lock<T>(state: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    state.lock().map_err(|_| Error::Lock)
}

#[cfg(test)]
mod tests {
    use std::panic::catch_unwind;
    use std::panic::AssertUnwindSafe;
    use std::sync::Mutex;

    use super::lock;
    use crate::error::Error;

    #[test]
    fn test_lock_acquires_unpoisoned_mutex_and_maps_poison_to_error_lock() {
        let state = Mutex::new(10_u32);
        {
            let mut guard = lock(&state).expect("unpoisoned mutex");
            *guard = 42;
        }
        assert_eq!(*lock(&state).expect("updated mutex"), 42);

        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let _guard = state.lock().expect("acquire before panic");
            panic!("poison mutex");
        }));
        assert!(poisoned.is_err());
        assert!(matches!(lock(&state), Err(Error::Lock)));
    }
}
