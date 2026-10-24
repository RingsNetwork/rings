use std::collections::BTreeSet;
use std::time::Duration;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry::EntryKind;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;
use rings_dns_relay::ServiceName;

use super::*;
use crate::dns_relay::descriptor::ServedTxt;
use crate::dns_relay::descriptor::ServiceDescriptorBody;
use crate::dns_relay::registration::ServiceOffer;
use crate::dns_relay::registration::ServiceRegistration;
use crate::dns_relay::relay_service;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::tests::native::prepare_processor;

/// An announcement serving port 25.
fn mail() -> Announcement {
    Announcement::new([(25, ServiceName::new(relay_service(25)).unwrap())], []).unwrap()
}

/// A did numbered `n`.
fn did(n: u32) -> Did {
    Did::from(n)
}

#[test]
fn test_cache_answers_fresh_fetches_negative_ones_included() {
    let mut cache = LookupCache::new(100, 8);
    assert_eq!(cache.get(&did(1), 0), None);
    cache.put(did(1), Some(mail()), 0);
    cache.put(did(2), None, 0);
    assert_eq!(cache.get(&did(1), 99), Some(Some(mail())));
    assert_eq!(cache.get(&did(2), 99), Some(None));
    assert_eq!(
        cache.get(&did(1), 100),
        None,
        "a fetch is stale after its TTL"
    );
}

#[test]
fn test_cache_never_exceeds_capacity_and_evicts_stale_then_oldest() {
    let mut cache = LookupCache::new(100, 2);
    cache.put(did(1), None, 0);
    cache.put(did(2), None, 150);
    // did(1) is stale at 160: it goes first, and did(2) stays.
    cache.put(did(3), None, 160);
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.get(&did(2), 160), Some(None));
    // Nothing is stale at 170: the oldest, did(2), goes.
    cache.put(did(4), None, 170);
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.get(&did(2), 170), None);
    assert_eq!(cache.get(&did(3), 170), Some(None));
    assert_eq!(cache.get(&did(4), 170), Some(None));
    // Refreshing a held owner evicts nothing.
    cache.put(did(3), Some(mail()), 171);
    assert_eq!(cache.entries.len(), 2);
}

/// The offer of port 25 and an ACME challenge record.
fn offer() -> ServiceOffer {
    ServiceOffer::new(
        BTreeSet::from([25]),
        vec![ServedTxt {
            name: "_acme-challenge".to_owned(),
            values: vec!["token".to_owned()],
        }],
        Duration::from_secs(30),
        Duration::from_secs(90),
    )
    .unwrap()
}

#[tokio::test]
async fn test_a_published_offer_is_read_back_as_its_announcement() {
    let processor = Arc::new(prepare_processor().await);
    let registration = ServiceRegistration::new(processor.did(), offer());
    registration
        .register_once(&RegistrationContext::new(&processor))
        .await
        .unwrap();
    let directory = DhtDirectory::new(Arc::clone(&processor));
    let announcement = directory
        .announcement(DidOwner(processor.did()))
        .await
        .unwrap()
        .expect("the published descriptor is found");
    assert_eq!(announcement.served_ports().collect::<Vec<_>>(), vec![(
        25,
        &ServiceName::new("dns-relay/25").unwrap()
    )]);
    assert!(announcement.receives_mail());
    assert_eq!(
        directory.announcement(DidOwner(did(7))).await.unwrap(),
        None,
        "an owner that publishes nothing announces nothing"
    );
}

#[tokio::test]
async fn test_a_descriptor_signed_for_another_did_does_not_speak_for_the_owner() {
    let processor = Arc::new(prepare_processor().await);
    let network_id = processor.swarm.network_id();
    let impostor_key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
    let impostor = impostor_key.delegator_did();
    let now_ms = get_epoch_ms();
    // Written on the owner's topic, but signed by, and for, the impostor.
    let forged = ServiceDescriptor::new_signed(
        ServiceDescriptorBody {
            did: impostor,
            public_key: impostor_key
                .delegation()
                .delegator_verification_pubkey()
                .unwrap(),
            network_id,
            ports: vec![25],
            txt: vec![],
            heartbeat_at_ms: now_ms,
            expires_at_ms: now_ms + 90_000,
        },
        MessageSigner::new(&impostor_key, network_id),
    )
    .unwrap();
    let topic = service_topic(processor.did());
    processor
        .storage_store(Entry::new(
            Entry::gen_did(&topic).unwrap(),
            vec![forged.to_element().unwrap()],
            EntryKind::Data,
        ))
        .await
        .unwrap();
    let directory = DhtDirectory::new(Arc::clone(&processor));
    assert_eq!(
        directory
            .announcement(DidOwner(processor.did()))
            .await
            .unwrap(),
        None
    );
}

#[test]
fn test_an_expired_descriptor_announces_nothing() {
    let key = DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap();
    let owner = key.delegator_did();
    let now_ms = get_epoch_ms();
    let expired = ServiceDescriptor::new_signed(
        ServiceDescriptorBody {
            did: owner,
            public_key: key.delegation().delegator_verification_pubkey().unwrap(),
            network_id: 0,
            ports: vec![25],
            txt: vec![],
            heartbeat_at_ms: now_ms - 200_000,
            expires_at_ms: now_ms - 110_000,
        },
        MessageSigner::new(&key, 0),
    )
    .unwrap();
    let elements = vec![expired.to_element().unwrap()];
    assert_eq!(announcement_in(&elements, owner, now_ms, 0), None);
    assert_eq!(
        announcement_in(&elements, owner, now_ms - 150_000, 0),
        Some(mail()),
        "the same descriptor is valid while live"
    );
}
