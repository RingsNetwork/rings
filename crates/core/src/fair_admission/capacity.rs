use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// Atomically add `amount` when the resulting reservation stays within `limit`.
pub(crate) fn try_reserve_atomic(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

pub(crate) fn fair_reservation_fits<const N: usize>(
    admitted_by_class: &[usize; N],
    admitted: usize,
    class_index: usize,
    amount: usize,
    capacity: usize,
    reservations: &[usize; N],
) -> bool {
    if class_index >= N {
        return false;
    }
    let reserved_for_others = admitted_by_class
        .iter()
        .zip(reservations)
        .enumerate()
        .filter(|(index, _)| *index != class_index)
        .map(|(_, (admitted, reserved))| reserved.saturating_sub(*admitted))
        .sum::<usize>();
    admitted
        .checked_add(amount)
        .is_some_and(|next| next <= capacity.saturating_sub(reserved_for_others))
}

const fn reserved_for_other_classes(reservations: &[usize], class_index: usize) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    if class_index == 0 {
        return reservation_sum(remaining);
    }
    first.saturating_add(reserved_for_other_classes(remaining, class_index - 1))
}

const fn reservation_sum(reservations: &[usize]) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    first.saturating_add(reservation_sum(remaining))
}

pub(crate) const fn admissible_capacity<const N: usize>(
    capacity: usize,
    reservations: &[usize; N],
    class_index: usize,
) -> usize {
    capacity.saturating_sub(reserved_for_other_classes(reservations, class_index))
}

pub(crate) const fn retained_wire_bytes(wire_bytes: usize) -> usize {
    wire_bytes.saturating_mul(2)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountedReservationRejection {
    Count,
    Bytes,
}

pub(crate) fn fixed_reservation_covers<const N: usize>(
    admitted_by_class: &[usize; N],
    class_index: usize,
    amount: usize,
    reservations: &[usize; N],
) -> bool {
    admitted_by_class
        .get(class_index)
        .zip(reservations.get(class_index))
        .and_then(|(admitted, reserved)| admitted.checked_add(amount).map(|next| (next, reserved)))
        .is_some_and(|(next, reserved)| next <= *reserved)
}

#[derive(Clone, Copy)]
pub(crate) struct ReservedCapacity<const N: usize> {
    admitted: usize,
    admitted_by_class: [usize; N],
}

impl<const N: usize> ReservedCapacity<N> {
    pub(crate) const fn new() -> Self {
        Self {
            admitted: 0,
            admitted_by_class: [0; N],
        }
    }

    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        amount: usize,
        reservations: &[usize; N],
    ) -> bool {
        fixed_reservation_covers(&self.admitted_by_class, class_index, amount, reservations)
    }

    pub(crate) fn try_reserve(
        &mut self,
        class_index: usize,
        amount: usize,
        capacity: usize,
        reservations: &[usize; N],
    ) -> bool {
        if !self.can_reserve(class_index, amount, capacity, reservations) {
            return false;
        }
        self.admitted = self.admitted.saturating_add(amount);
        if let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) {
            *class_admitted = class_admitted.saturating_add(amount);
        }
        true
    }

    pub(crate) fn can_reserve(
        &self,
        class_index: usize,
        amount: usize,
        capacity: usize,
        reservations: &[usize; N],
    ) -> bool {
        fair_reservation_fits(
            &self.admitted_by_class,
            self.admitted,
            class_index,
            amount,
            capacity,
            reservations,
        )
    }

    pub(crate) fn release(&mut self, class_index: usize, amount: usize) {
        let Some(next_class) = self
            .admitted_by_class
            .get(class_index)
            .and_then(|admitted| admitted.checked_sub(amount))
        else {
            return;
        };
        let Some(next_total) = self.admitted.checked_sub(amount) else {
            return;
        };
        let Some(class_admitted) = self.admitted_by_class.get_mut(class_index) else {
            return;
        };
        *class_admitted = next_class;
        self.admitted = next_total;
    }

    pub(crate) const fn admitted(self) -> usize {
        self.admitted
    }

    #[cfg(test)]
    pub(super) const fn admitted_by_class(&self) -> &[usize; N] {
        &self.admitted_by_class
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CountedReservedCapacity<const N: usize> {
    counts: ReservedCapacity<N>,
    bytes: ReservedCapacity<N>,
}

impl<const N: usize> CountedReservedCapacity<N> {
    pub(crate) const fn new() -> Self {
        Self {
            counts: ReservedCapacity::new(),
            bytes: ReservedCapacity::new(),
        }
    }

    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        bytes: usize,
        count_reservations: &[usize; N],
        byte_reservations: &[usize; N],
    ) -> bool {
        self.counts
            .reservation_covers(class_index, 1, count_reservations)
            && self
                .bytes
                .reservation_covers(class_index, bytes, byte_reservations)
    }

    pub(crate) fn try_reserve(
        &mut self,
        class_index: usize,
        bytes: usize,
        count_capacity: usize,
        count_reservations: &[usize; N],
        byte_capacity: usize,
        byte_reservations: &[usize; N],
    ) -> Result<(), CountedReservationRejection> {
        if !self
            .counts
            .try_reserve(class_index, 1, count_capacity, count_reservations)
        {
            return Err(CountedReservationRejection::Count);
        }
        if !self
            .bytes
            .try_reserve(class_index, bytes, byte_capacity, byte_reservations)
        {
            self.counts.release(class_index, 1);
            return Err(CountedReservationRejection::Bytes);
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, class_index: usize, bytes: usize) {
        self.counts.release(class_index, 1);
        self.bytes.release(class_index, bytes);
    }

    pub(crate) const fn admitted_count(self) -> usize {
        self.counts.admitted()
    }

    #[cfg(test)]
    pub(crate) const fn admitted_bytes(self) -> usize {
        self.bytes.admitted()
    }
}

impl<const N: usize> Default for CountedReservedCapacity<N> {
    fn default() -> Self {
        Self::new()
    }
}
