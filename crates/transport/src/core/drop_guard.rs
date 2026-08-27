//! Shared one-shot cleanup guard.

/// Runs one cleanup action at most once, either explicitly or when dropped.
///
/// The guarded value and action are consumed together. Calling [`Self::fire`]
/// executes the action immediately, [`Self::take`] transfers the value without
/// executing the action, and [`Self::disarm`] drops both without executing it.
pub struct ArmedDropGuard<T, F>
where F: FnOnce(T)
{
    value: Option<T>,
    action: Option<F>,
}

impl<T, F> ArmedDropGuard<T, F>
where F: FnOnce(T)
{
    /// Arm `action` to receive `value` when the guard fires or is dropped.
    pub fn new(value: T, action: F) -> Self {
        Self {
            value: Some(value),
            action: Some(action),
        }
    }

    /// Execute the cleanup action now and disarm the guard.
    pub fn fire(&mut self) {
        if let (Some(value), Some(action)) = (self.value.take(), self.action.take()) {
            action(value);
        }
    }

    /// Transfer the guarded value without executing the cleanup action.
    pub fn take(&mut self) -> Option<T> {
        self.action = None;
        self.value.take()
    }

    /// Drop the guarded value and cleanup action without executing the action.
    pub fn disarm(&mut self) {
        self.action = None;
        self.value = None;
    }

    /// Borrow the guarded value while the cleanup action remains armed.
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Mutably borrow the guarded value while the cleanup action remains armed.
    pub fn value_mut(&mut self) -> Option<&mut T> {
        self.value.as_mut()
    }
}

impl<T, F> Drop for ArmedDropGuard<T, F>
where F: FnOnce(T)
{
    fn drop(&mut self) {
        self.fire();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::ArmedDropGuard;

    #[test]
    fn test_drop_fires_action_exactly_once() {
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let guard = ArmedDropGuard::new(2, move |amount| {
            observed.set(observed.get() + amount);
        });

        drop(guard);

        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn test_explicit_fire_consumes_drop_authority() {
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let mut guard = ArmedDropGuard::new(3, move |amount| {
            observed.set(observed.get() + amount);
        });

        guard.fire();
        drop(guard);

        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn test_disarm_suppresses_action() {
        let fired = Rc::new(Cell::new(false));
        let observed = Rc::clone(&fired);
        let mut guard = ArmedDropGuard::new((), move |()| observed.set(true));

        guard.disarm();
        drop(guard);

        assert!(!fired.get());
    }
}
