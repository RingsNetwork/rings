use std::collections::BTreeSet;
use std::net::Ipv6Addr;

use ipnet::Ipv6Net;

use super::*;
use crate::owner::test_owner::TestOwner;

/// The key whose every byte is `byte`.
fn key(byte: u8) -> AddressKey {
    AddressKey::from_bytes(Zeroizing::new([byte; ADDRESS_KEY_LEN]))
}

/// The prefix `prefix`.
fn prefix(prefix: &str) -> Ipv6Net {
    prefix.parse().unwrap()
}

/// The address numbered `n`.
fn address(n: u128) -> Ipv6Addr {
    Ipv6Addr::from(n)
}

/// The binding of `address` to owner `n`, answered at `at`.
fn bound(address: Ipv6Addr, n: u32, at: u64) -> BookChange<TestOwner> {
    BookChange::Bind {
        address,
        owner: TestOwner(n),
        answered_at_ms: at,
    }
}

#[test]
fn test_address_of_lies_in_the_prefix_and_is_deterministic() {
    for (length, text) in [
        (64, "2001:db8:1::/64"),
        (96, "2001:db8:2::/96"),
        (120, "fd00::/120"),
    ] {
        let plan = AddressPlan::new(prefix(text), key(1));
        let again = AddressPlan::new(prefix(text), key(1));
        for n in 0..256 {
            let owner = TestOwner(n);
            let address = plan.address_of(&owner);
            assert!(plan.prefix().contains(&address), "/{length}: {address}");
            assert_eq!(address, again.address_of(&owner));
        }
    }
}

#[test]
fn test_address_of_ignores_host_bits_given_in_the_prefix() {
    let plan = AddressPlan::new(prefix("2001:db8:1::5/64"), key(1));
    assert_eq!(plan.prefix(), prefix("2001:db8:1::/64"));
}

#[test]
fn test_address_of_spreads_owners_and_depends_on_the_key() {
    let plan = AddressPlan::new(prefix("2001:db8:1::/64"), key(1));
    let other = AddressPlan::new(prefix("2001:db8:1::/64"), key(2));
    let owners = (0..1024).map(TestOwner).collect::<Vec<_>>();
    let addresses = owners
        .iter()
        .map(|owner| plan.address_of(owner))
        .collect::<BTreeSet<Ipv6Addr>>();
    assert_eq!(
        addresses.len(),
        owners.len(),
        "64 host bits: no collision in 1024"
    );
    let moved = owners
        .iter()
        .filter(|owner| plan.address_of(*owner) != other.address_of(*owner))
        .count();
    assert_eq!(moved, owners.len(), "another key moves every owner");
}

#[test]
fn test_the_key_keeps_its_bytes() {
    assert_eq!(key(9).as_bytes(), [9u8; ADDRESS_KEY_LEN].as_slice());
}

#[test]
fn test_bind_persists_a_fresh_binding_and_renewals_past_the_refresh_only() {
    // Retention 800: a renewal is persisted once it moved more than 100.
    let mut book = AddressBook::new(800, 8);
    assert_eq!(
        book.bind(address(1), TestOwner(1), 10),
        Ok(vec![bound(address(1), 1, 10)])
    );
    assert_eq!(book.bind(address(1), TestOwner(1), 110), Ok(vec![]));
    assert_eq!(
        book.bind(address(1), TestOwner(1), 111),
        Ok(vec![bound(address(1), 1, 111)])
    );
    assert_eq!(
        book.bind(address(1), TestOwner(1), 50),
        Ok(vec![]),
        "time never moves back"
    );
    assert_eq!(book.owner_at(&address(1)), Some(TestOwner(1)));
}

#[test]
fn test_bind_refuses_a_second_owner_and_leaves_the_book_unchanged() {
    let mut book = AddressBook::new(800, 8);
    book.bind(address(1), TestOwner(1), 10).unwrap();
    let before = book.clone();
    assert_eq!(
        book.bind(address(1), TestOwner(2), 20),
        Err(AddressCollision {
            address: address(1)
        })
    );
    assert_eq!(book, before);
}

#[test]
fn test_a_full_book_forgets_the_expired_then_the_least_recently_answered() {
    let mut book = AddressBook::new(100, 2);
    book.bind(address(1), TestOwner(1), 0).unwrap();
    book.bind(address(2), TestOwner(2), 150).unwrap();
    // At 160, owner 1 is past its retention: it goes, owner 2 stays.
    assert_eq!(
        book.bind(address(3), TestOwner(3), 160),
        Ok(vec![
            BookChange::Unbind(address(1)),
            bound(address(3), 3, 160)
        ])
    );
    // Nothing is expired at 170: the least recently answered, owner 2, goes.
    assert_eq!(
        book.bind(address(4), TestOwner(4), 170),
        Ok(vec![
            BookChange::Unbind(address(2)),
            bound(address(4), 4, 170)
        ])
    );
    assert_eq!(book.len(), 2);
    assert_eq!(book.owner_at(&address(3)), Some(TestOwner(3)));
    assert_eq!(book.owner_at(&address(4)), Some(TestOwner(4)));
}

#[test]
fn test_restore_keeps_the_first_owner_of_an_address_and_the_most_recent_within_capacity() {
    let book = AddressBook::restore(800, 2, [
        (address(1), TestOwner(1), 50),
        (address(1), TestOwner(2), 60),
        (address(2), TestOwner(3), 10),
        (address(3), TestOwner(4), 40),
    ]);
    assert_eq!(
        book.owner_at(&address(1)),
        Some(TestOwner(1)),
        "the first owner stands"
    );
    assert_eq!(
        book.owner_at(&address(2)),
        None,
        "the least recently answered goes"
    );
    assert_eq!(book.owner_at(&address(3)), Some(TestOwner(4)));
    assert_eq!(book.bindings().count(), 2);
}
