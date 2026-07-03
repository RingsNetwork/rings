//! Generation tokens for cancelling stale UI writes.

use std::cell::RefCell;
use std::rc::Rc;

/// Shared monotonically increasing UI generation counter.
#[derive(Clone, Default)]
pub(crate) struct GenerationClock {
    current: Rc<RefCell<u64>>,
}

/// Proof that an async operation still owns the generation it started in.
#[derive(Clone)]
pub(crate) struct GenerationToken {
    current: Rc<RefCell<u64>>,
    generation: u64,
}

impl GenerationClock {
    pub(crate) fn bump(&self) -> GenerationToken {
        let mut current = self.current.borrow_mut();
        *current = current.wrapping_add(1);
        GenerationToken {
            current: self.current.clone(),
            generation: *current,
        }
    }

    pub(crate) fn token(&self) -> GenerationToken {
        GenerationToken {
            current: self.current.clone(),
            generation: *self.current.borrow(),
        }
    }
}

impl GenerationToken {
    pub(crate) fn is_current(&self) -> bool {
        *self.current.borrow() == self.generation
    }
}
