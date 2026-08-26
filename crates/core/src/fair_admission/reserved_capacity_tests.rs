use super::admissible_capacity;
use super::ReservedCapacity;

#[test]
fn admissible_capacity_preserves_every_other_class_reservation() {
    let reservations = [1, 2, 3];

    assert_eq!(admissible_capacity(10, &reservations, 0), 5);
    assert_eq!(admissible_capacity(10, &reservations, 1), 6);
    assert_eq!(admissible_capacity(10, &reservations, 2), 7);
    assert_eq!(admissible_capacity(10, &reservations, 3), 4);
}

#[test]
fn invalid_release_keeps_aggregate_and_class_totals_equal() {
    let mut capacity = ReservedCapacity::<2>::new();
    assert!(capacity.try_reserve(0, 4, 8, &[0, 0]));
    capacity.release(2, 1);
    assert_eq!(capacity.admitted, 4);
    assert_eq!(capacity.admitted_by_class, [4, 0]);
    capacity.release(0, 5);
    assert_eq!(capacity.admitted, 4);
    assert_eq!(capacity.admitted_by_class, [4, 0]);
}

#[test]
fn valid_release_preserves_capacity_sum_invariant() {
    let mut capacity = ReservedCapacity::<2>::new();
    assert!(capacity.try_reserve(0, 3, 8, &[0, 0]));
    assert!(capacity.try_reserve(1, 2, 8, &[0, 0]));
    capacity.release(0, 2);
    assert_eq!(capacity.admitted, 3);
    assert_eq!(capacity.admitted_by_class.iter().sum::<usize>(), 3);
}
