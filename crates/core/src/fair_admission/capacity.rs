/// Return whether admitting `amount` for `class_index` fits within `capacity` while preserving
/// every other class's unmet minimum reservation.
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

/// Sum the reservations of all classes other than `class_index`.
const fn reserved_for_other_classes(reservations: &[usize], class_index: usize) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    if class_index == 0 {
        return reservation_sum(remaining);
    }
    first.saturating_add(reserved_for_other_classes(remaining, class_index - 1))
}

/// Sum all entries in `reservations` with saturating addition.
const fn reservation_sum(reservations: &[usize]) -> usize {
    let Some((first, remaining)) = reservations.split_first() else {
        return 0;
    };
    first.saturating_add(reservation_sum(remaining))
}

/// Maximum capacity admissible to `class_index` after subtracting all other classes' reservations.
pub(crate) const fn admissible_capacity<const N: usize>(
    capacity: usize,
    reservations: &[usize; N],
    class_index: usize,
) -> usize {
    capacity.saturating_sub(reserved_for_other_classes(reservations, class_index))
}

/// Conservative retained-memory estimate for a `wire_bytes` payload (twice the wire size).
pub(crate) const fn retained_wire_bytes(wire_bytes: usize) -> usize {
    wire_bytes.saturating_mul(2)
}

/// Dimension that rejected a [`CountedReservedCapacity::try_reserve`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CountedReservationRejection {
    /// Item count capacity or reservation limit was exceeded.
    Count,
    /// Byte capacity or reservation limit was exceeded.
    Bytes,
}

/// Return whether `class_index` can admit `amount` strictly within its own fixed reservation.
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

/// Tracks total and per-class admitted units across `N` admission classes.
#[derive(Clone, Copy)]
pub(crate) struct ReservedCapacity<const N: usize> {
    /// Total admitted units across all classes.
    admitted: usize,
    /// Admitted units indexed by class.
    admitted_by_class: [usize; N],
}

impl<const N: usize> ReservedCapacity<N> {
    /// Create an empty capacity tracker with zero admitted units.
    pub(crate) const fn new() -> Self {
        Self {
            admitted: 0,
            admitted_by_class: [0; N],
        }
    }

    /// Return whether `amount` fits within `class_index`'s own fixed reservation.
    pub(crate) fn reservation_covers(
        &self,
        class_index: usize,
        amount: usize,
        reservations: &[usize; N],
    ) -> bool {
        fixed_reservation_covers(&self.admitted_by_class, class_index, amount, reservations)
    }

    /// Attempt to reserve `amount` for `class_index`, returning `true` on success.
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

    /// Return whether `amount` can currently be reserved for `class_index` under `capacity` and `reservations`.
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

    /// Release `amount` previously reserved for `class_index`.
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

    /// Return the total admitted units across all classes.
    pub(crate) const fn admitted(self) -> usize {
        self.admitted
    }

    #[cfg(test)]
    pub(super) const fn admitted_by_class(&self) -> &[usize; N] {
        &self.admitted_by_class
    }
}

/// Jointly tracks item-count and byte reservations across `N` admission classes.
#[derive(Clone, Copy)]
pub(crate) struct CountedReservedCapacity<const N: usize> {
    /// Item-count capacity tracker.
    counts: ReservedCapacity<N>,
    /// Byte-volume capacity tracker.
    bytes: ReservedCapacity<N>,
}

impl<const N: usize> CountedReservedCapacity<N> {
    /// Create an empty counted capacity tracker.
    pub(crate) const fn new() -> Self {
        Self {
            counts: ReservedCapacity::new(),
            bytes: ReservedCapacity::new(),
        }
    }

    /// Return whether one item of `bytes` fits within `class_index`'s fixed count and byte reservations.
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

    /// Atomically reserve one item and `bytes` for `class_index`, rolling back count on byte rejection.
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

    /// Release one item and `bytes` previously reserved for `class_index`.
    pub(crate) fn release(&mut self, class_index: usize, bytes: usize) {
        self.counts.release(class_index, 1);
        self.bytes.release(class_index, bytes);
    }

    /// Return the total number of currently admitted items across all classes.
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
