use super::*;
use crate::dht::tests::gen_ordered_dids;

#[test]
fn test_finger_table_size_bounds() {
    let did = gen_ordered_dids(1)[0];

    assert_eq!(
        FingerTable::new(did, DEFAULT_FINGER_TABLE_SIZE + 1).slot_count(),
        DEFAULT_FINGER_TABLE_SIZE
    );
    assert_eq!(FingerTable::new(did, 0).slot_count(), 0);
}

#[test]
fn test_finger_table_get_set_remove() {
    let dids = gen_ordered_dids(5);

    let mut table = FingerTable::new(dids[0], 3);
    println!("check finger len");
    assert_eq!(table.len(), 0);
    assert_eq!(table.finger.len(), 3);
    println!("check finger all items is none");
    assert!(table.get(0).is_none(), "index 0 should be None");
    assert!(table.get(1).is_none(), "index 1 should be None");
    assert!(table.get(2).is_none(), "index 2 should be None");
    assert!(table.get(3).is_none(), "index 3 should be None");

    println!("set finger item");
    let (id1, id2, id3, id4) = (dids[1], dids[2], dids[3], dids[4]);

    table.set(0, id1);
    assert_eq!(table.len(), 1);
    assert_eq!(table.finger.len(), 3);

    table.set(2, id3);
    assert_eq!(table.len(), 2);
    assert_eq!(table.finger.len(), 3);

    assert!(
        table.get(0) == Some(id1),
        "expect value at index 0 is {:?}, got {:?}",
        Some(id1),
        table.get(0)
    );
    assert!(
        table.get(1).is_none(),
        "expect value at index 1 is None, got {:?}",
        table.get(1)
    );
    assert!(
        table.get(2) == Some(id3),
        "expect value at index 2 is {:?}, got {:?}",
        Some(id3),
        table.get(2)
    );

    println!("set value out of index");
    table.set(4, id4);
    assert_eq!(table.len(), 2);
    assert_eq!(table.finger.len(), 3);

    println!("remove node from finger");
    table.remove(id1);
    assert_eq!(table.len(), 1);
    assert_eq!(table.finger.len(), 3);
    assert!(
        table.get(0).is_none(),
        "expect value at index 1 is None, got {:?}",
        table.get(0)
    );
    assert!(
        table.get(2) == Some(id3),
        "expect value at index 2 is {:?}, got {:?}",
        Some(id3),
        table.get(2)
    );

    println!("remove node with auto fill");
    table.set(0, id1);
    table.set(1, id2);
    assert!(
        table.get(0) == Some(id1),
        "expect value at index 0 is {:?}, got {:?}",
        Some(id1),
        table.get(0)
    );
    assert!(
        table.get(1) == Some(id2),
        "expect value at index 1 is {:?}, got {:?}",
        Some(id2),
        table.get(1)
    );
    assert!(
        table.get(2) == Some(id3),
        "expect value at index 2 is {:?}, got {:?}",
        Some(id3),
        table.get(2)
    );

    table.remove(id1);
    assert_eq!(table.len(), 3);
    assert_eq!(table.finger.len(), 3);
    assert!(
        table.get(0) == Some(id2),
        "expect value at index 0 is {:?}, got {:?}",
        id2,
        table.get(0)
    );
    assert!(
        table.get(1) == Some(id2),
        "expect value at index 1 is {:?}, got {:?}",
        Some(id2),
        table.get(1),
    );

    println!("remove item not in fingers");
    table.remove(id4);

    println!("remove all items in fingers");
    table.remove(id1);
    assert_eq!(table.first(), Some(id2));

    println!("check first item");
    table.remove(id3);
    assert_eq!(table.first(), Some(id2));

    table.remove(id2);
    assert_eq!(table.first(), None);
    assert_eq!(table.len(), 0);
    assert_eq!(table.finger.len(), 3);
}

#[test]
fn test_finger_table_remove_then_fill() {
    let dids = gen_ordered_dids(6);
    let (did1, did2, did3, did4, did5) = (dids[1], dids[2], dids[3], dids[4], dids[5]);

    let mut table = FingerTable::new(dids[0], 5);

    // [did1, did2, did3, did4, did5] - did1 = [did2, did2, did3, did4, did5]
    table.reset_finger();
    table.set(0, did1);
    table.set(1, did2);
    table.set(2, did3);
    table.set(3, did4);
    table.set(4, did5);
    table.remove(did1);
    assert_eq!(table.finger, [
        Some(did2),
        Some(did2),
        Some(did3),
        Some(did4),
        Some(did5),
    ]);

    // [did1, did2, did3, did4, did5] - did2 = [did1, did3, did3, did4, did5]
    table.reset_finger();
    table.set(0, did1);
    table.set(1, did2);
    table.set(2, did3);
    table.set(3, did4);
    table.set(4, did5);
    table.remove(did2);
    assert_eq!(table.finger, [
        Some(did1),
        Some(did3),
        Some(did3),
        Some(did4),
        Some(did5),
    ]);

    // [did1, None, did3, did4, did5] - did1 = [None, None, did3, did4, did5]
    table.reset_finger();
    table.set(0, did1);
    table.set(2, did3);
    table.set(3, did4);
    table.set(4, did5);
    table.remove(did1);
    assert_eq!(table.finger, [
        None,
        None,
        Some(did3),
        Some(did4),
        Some(did5),
    ]);

    // [did1, None, did3, did4, did5] - did3 = [did1, None, did4, did4, did5]
    table.reset_finger();
    table.set(0, did1);
    table.set(2, did3);
    table.set(3, did4);
    table.set(4, did5);
    table.remove(did3);
    assert_eq!(table.finger, [
        Some(did1),
        None,
        Some(did4),
        Some(did4),
        Some(did5),
    ]);

    // [did1, did2, did3, did4, did5] - did5 = [did1, did2, did4, did4, None]
    table.reset_finger();
    table.set(0, did1);
    table.set(1, did2);
    table.set(2, did3);
    table.set(3, did4);
    table.set(4, did5);
    table.remove(did5);
    assert_eq!(table.finger, [
        Some(did1),
        Some(did2),
        Some(did3),
        Some(did4),
        None
    ]);

    // A partially repaired table may contain non-contiguous runs for one
    // peer. Removing it must not erase the valid slots between those runs.
    table.reset_finger();
    table.set(0, did1);
    table.set(1, did2);
    table.set(2, did1);
    table.set(3, did4);
    table.remove(did1);
    assert_eq!(table.finger, [
        Some(did2),
        Some(did2),
        Some(did4),
        Some(did4),
        None
    ]);
}
