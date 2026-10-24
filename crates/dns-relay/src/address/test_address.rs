use std::collections::BTreeSet;
use std::net::Ipv6Addr;

use ipnet::Ipv6Net;

use super::*;
use crate::owner::test_owner::TestOwner;

/// The key whose every byte is `byte`.
fn key(byte: u8) -> AddressKey {
    AddressKey::from_bytes([byte; ADDRESS_KEY_LEN])
}

/// The prefix `prefix`.
fn prefix(prefix: &str) -> Ipv6Net {
    prefix.parse().unwrap()
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
fn test_bind_is_fresh_then_renewed_and_keeps_the_latest_answer() {
    let address = Ipv6Addr::LOCALHOST;
    let mut book = AddressBook::default();
    assert_eq!(book.bind(address, TestOwner(1), 10), Ok(Bound::Fresh));
    assert_eq!(book.bind(address, TestOwner(1), 30), Ok(Bound::Renewed));
    assert_eq!(book.bind(address, TestOwner(1), 20), Ok(Bound::Renewed));
    assert_eq!(book.owner_at(&address), Some(TestOwner(1)));
    assert_eq!(
        book.bindings()
            .map(|(_, binding)| binding.answered_at_ms)
            .collect::<Vec<_>>(),
        vec![30]
    );
}

#[test]
fn test_bind_refuses_a_second_owner_and_leaves_the_book_unchanged() {
    let address = Ipv6Addr::LOCALHOST;
    let mut book = AddressBook::default();
    book.bind(address, TestOwner(1), 10).unwrap();
    let before = book.clone();
    assert_eq!(
        book.bind(address, TestOwner(2), 20),
        Err(AddressCollision { address })
    );
    assert_eq!(book, before);
}

#[test]
fn test_forget_answered_before_keeps_the_recent_bindings() {
    let mut book = AddressBook::default();
    let old = Ipv6Addr::from(1u128);
    let recent = Ipv6Addr::from(2u128);
    book.bind(old, TestOwner(1), 10).unwrap();
    book.bind(recent, TestOwner(2), 20).unwrap();
    assert!(!book.forget_answered_before(10));
    assert!(book.forget_answered_before(15));
    assert_eq!(book.owner_at(&old), None);
    assert_eq!(book.owner_at(&recent), Some(TestOwner(2)));
}

#[test]
fn test_from_bindings_keeps_the_first_owner_of_an_address() {
    let address = Ipv6Addr::LOCALHOST;
    let binding = |owner, answered_at_ms| Binding {
        owner: TestOwner(owner),
        answered_at_ms,
    };
    let book = AddressBook::from_bindings([(address, binding(1, 10)), (address, binding(2, 20))]);
    assert_eq!(book.owner_at(&address), Some(TestOwner(1)));
}
