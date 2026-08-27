use std::str::FromStr;

use num_bigint::BigUint;

use super::*;
use crate::ecc::SecretKey;

#[tokio::test]
async fn test_two_nodes_install_each_other_in_successors_and_fingers() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() > key2.address() {
        (key1, key2) = (key2, key1);
    }
    let did1: Did = key1.address().into();
    let did2: Did = key2.address().into();
    let node1 = PeerRing::new_with_storage(did1, 3, Box::new(MemStorage::new()));
    let node2 = PeerRing::new_with_storage(did2, 3, Box::new(MemStorage::new()));

    node1.join(did2)?;
    node2.join(did1)?;
    assert!(node1.successors().list()?.contains(&did2));
    assert!(node2.successors().list()?.contains(&did1));
    assert!(node1.lock_finger()?.contains(Some(did2)));
    assert!(node2.lock_finger()?.contains(Some(did1)));
    Ok(())
}

#[tokio::test]
async fn test_two_node_wraparound_installs_expected_fingers() -> Result<()> {
    let did1 = Did::from_str("0x051cf4f8d020cb910474bef3e17f153fface2b5f").unwrap();
    let did2 = Did::from_str("0x54baa7dc9e28f41da5d71af8fa6f2a302be1c1bf").unwrap();
    let max = Did::from(BigUint::from(2u16).pow(160) - 1u16);
    let zero = Did::from(BigUint::from(2u16).pow(160));

    let node1 = PeerRing::new_with_storage(did1, 3, Box::new(MemStorage::new()));
    let node2 = PeerRing::new_with_storage(did2, 3, Box::new(MemStorage::new()));
    node1.join(did2)?;
    node2.join(did1)?;

    assert!(node1.successors().list()?.contains(&did2));
    assert!(node2.successors().list()?.contains(&did1));
    let pos_159 = did2 + Did::from(BigUint::from(2u16).pow(159));
    assert!(pos_159 > did2);
    assert!(pos_159 < max);
    let pos_160 = did2 + zero;
    assert_eq!(pos_160, did2);
    assert!(pos_160 > did1);
    assert!(node1.lock_finger()?.contains(Some(did2)));
    assert!(node2.lock_finger()?.contains(Some(did1)));
    Ok(())
}
