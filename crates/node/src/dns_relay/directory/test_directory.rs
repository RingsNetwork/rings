use std::collections::BTreeMap;
use std::time::Duration;

use rings_core::delegation::DelegateeKey;
use rings_core::dht::entry::EntryKind;
use rings_core::ecc::SecretKey;
use rings_core::message::MessageSigner;
use rings_dns_relay::ProxyProtocol;

use super::*;
use crate::dns_relay::descriptor::ServedPort;
use crate::dns_relay::descriptor::ServedTxt;
use crate::dns_relay::descriptor::ServiceDescriptorBody;
use crate::dns_relay::registration::ServiceOffer;
use crate::dns_relay::registration::ServiceRegistration;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::tests::native::prepare_processor;

/// An announcement serving port 25, with a PROXY protocol header.
fn mail() -> Announcement {
    Announcement::new([(25, ProxyProtocol::V2)], []).unwrap()
}

/// Port 25, as a descriptor serves it in [`mail`].
const MAIL_PORT: ServedPort = ServedPort {
    port: 25,
    proxy_protocol: ProxyProtocol::V2,
};

/// The offer of port 25 and an ACME challenge record.
fn offer() -> ServiceOffer {
    ServiceOffer::new(
        BTreeMap::from([(25, ProxyProtocol::V2)]),
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
        ProxyProtocol::V2
    )]);
    assert!(announcement.receives_mail());
    assert_eq!(
        directory
            .announcement(DidOwner(Did::from(7u32)))
            .await
            .unwrap(),
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
            ports: vec![MAIL_PORT],
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
            ports: vec![MAIL_PORT],
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
