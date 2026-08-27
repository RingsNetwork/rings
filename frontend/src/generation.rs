//! Generation tokens for cancelling stale UI writes.

use std::cell::RefCell;
use std::rc::Rc;

/// Shared UI ownership capability.
///
/// A fresh allocation replaces the current owner on every [`GenerationClock::bump`]. Pointer
/// identity cannot be reused while a stale token still exists, so cancellation does not depend on
/// a finite integer counter eventually wrapping.
#[derive(Clone, Default)]
pub(crate) struct GenerationClock {
    current: Rc<RefCell<Rc<()>>>,
}

/// Proof that an async operation still owns the generation it started in.
#[derive(Clone)]
pub(crate) struct GenerationToken {
    current: Rc<RefCell<Rc<()>>>,
    owner: Rc<()>,
}

impl GenerationClock {
    pub(crate) fn bump(&self) -> GenerationToken {
        let owner = Rc::new(());
        *self.current.borrow_mut() = owner.clone();
        GenerationToken {
            current: self.current.clone(),
            owner,
        }
    }

    pub(crate) fn token(&self) -> GenerationToken {
        GenerationToken {
            current: self.current.clone(),
            owner: self.current.borrow().clone(),
        }
    }
}

impl GenerationToken {
    pub(crate) fn is_current(&self) -> bool {
        Rc::ptr_eq(&self.current.borrow(), &self.owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bump_revokes_every_previous_owner_without_counter_reuse() {
        let clock = GenerationClock::default();
        let initial = clock.token();
        let first = clock.bump();
        let second = clock.bump();

        assert!(!initial.is_current());
        assert!(!first.is_current());
        assert!(second.is_current());
        assert!(clock.token().is_current());
    }
}
