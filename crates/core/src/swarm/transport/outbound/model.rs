use std::ops::Index;
use std::ops::IndexMut;
use std::slice::IterMut;

pub(in crate::swarm::transport) use crate::message::MessageCategory as TransferClass;
pub(in crate::swarm::transport) use crate::message::MessageKind as OutboundMessageKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) enum OutboundCompletion {
    Detached,
    Tracked,
}

/// One value per transfer class, read and written by the class itself: indexing is total, so
/// no lookup of a class's slot can miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::swarm::transport) struct PerClass<T>([T; TransferClass::COUNT]);

impl<T> PerClass<T> {
    /// The value `value(class)` of every class.
    pub(in crate::swarm::transport) fn from_fn(mut value: impl FnMut(TransferClass) -> T) -> Self {
        Self([
            value(TransferClass::DhtControl),
            value(TransferClass::Storage),
            value(TransferClass::E2e),
            value(TransferClass::Application),
        ])
    }

    /// Every class's value, mutably.
    pub(in crate::swarm::transport) fn values_mut(&mut self) -> IterMut<'_, T> {
        self.0.iter_mut()
    }
}

/// The value of `class` in `values`, which holds one per class in index order: total, unlike
/// an index by `class.index()`.
pub(in crate::swarm::transport) const fn of_class<T>(
    values: &[T; TransferClass::COUNT],
    class: TransferClass,
) -> &T {
    let [control, storage, e2e, application] = values;
    match class {
        TransferClass::DhtControl => control,
        TransferClass::Storage => storage,
        TransferClass::E2e => e2e,
        TransferClass::Application => application,
    }
}

impl<T> Index<TransferClass> for PerClass<T> {
    type Output = T;

    fn index(&self, class: TransferClass) -> &T {
        of_class(&self.0, class)
    }
}

impl<T> IndexMut<TransferClass> for PerClass<T> {
    fn index_mut(&mut self, class: TransferClass) -> &mut T {
        let [control, storage, e2e, application] = &mut self.0;
        match class {
            TransferClass::DhtControl => control,
            TransferClass::Storage => storage,
            TransferClass::E2e => e2e,
            TransferClass::Application => application,
        }
    }
}
