//! Mail through a gateway over the dummy transport (#1117): a recipient publishes its mail
//! descriptor, a gateway finds it and writes a sealed mail to the recipient's topic, and the
//! recipient fetches and opens it, every wait woken by the overlay's activity.
//!
//! ```text
//!   recipient R ─register μ(R)─▶ DHT ◀─recipient(R)─ gateway G
//!   gateway G   ─Extend(mail_topic(R), seal(m), MAX_TTL_MS)─▶ DHT ◀─poll─ recipient R
//! ```

use std::sync::Arc;

use bytes::Bytes;
use rand::rngs::OsRng;
use rings_core::consts::MAX_TTL_MS;
use rings_core::dht::entry::Entry;
use rings_core::dht::entry::EntryKind;
use rings_core::ecc::SecretKey;
use rings_core::storage::MemStorage;
use rings_core::utils::get_epoch_ms;

use super::common::connect_processors;
use super::common::network_test_guard;
use super::common::owns_entry_placement;
use super::common::prepare_processor_with_identity_key_network_and_virtual_nodes;
use super::common::wait_for_mutual_dht_topology;
use super::common::NetworkTestGuard;
use super::common::PROCESSOR_TEST_HANG_GUARD;
use crate::consts::DATA_REDUNDANT;
use crate::error::Error;
use crate::error::Result;
use crate::mail::descriptor::descriptor_topic;
use crate::mail::descriptor::MailRecipient;
use crate::mail::envelope::seal;
use crate::mail::envelope::MailElement;
use crate::mail::envelope::SealedMail;
use crate::mail::gateway::MailDrop;
use crate::mail::gateway::MailGateway;
use crate::mail::mail_topic;
use crate::mail::open::open_mail;
use crate::mail::open::Opening;
use crate::mail::recipient::KvInbox;
use crate::mail::recipient::PollReport;
use crate::mail::recipient::Recipient;
use crate::mail::recipient::RecipientRecord;
use crate::mail::recipient::StoredMail;
use crate::mail::recipient::DEFAULT_MAIL_POLL_INTERVAL;
use crate::mail::registration::MailOffer;
use crate::mail::registration::MailRegistration;
use crate::prelude::ChordStorageInterface;
use crate::processor::Processor;
use crate::registration::RegistrationContext;
use crate::registration::RegistrationTask;
use crate::tests::activity::probe_on_activity;

/// A message with a `Message-ID`.
const MESSAGE: &[u8] = b"Message-ID: <overlay@example.org>\r\n\r\nAcross the Great Wall.\r\n";

/// The recipient of these tests: its inbox and records in memory.
type MemRecipient = Recipient<KvInbox<MemStorage<StoredMail>>, MemStorage<RecipientRecord>>;

/// A gateway node and a recipient node, connected, each in the other's DHT view. Without
/// storage virtual nodes, each placement is owned by its successor, which
/// [`await_owners_holding`] reads.
async fn connected_pair() -> Result<(NetworkTestGuard, Arc<Processor>, Arc<Processor>)> {
    let network = network_test_guard().await;
    let node =
        || prepare_processor_with_identity_key_network_and_virtual_nodes(SecretKey::random(), 0, 0);
    let gateway = Arc::new(node().await);
    let recipient = Arc::new(node().await);
    connect_processors(&gateway, &recipient).await;
    wait_for_mutual_dht_topology(&gateway, &recipient).await?;
    Ok((network, gateway, recipient))
}

/// Await, on activity, the owner of every placement of `topic` among `nodes` holding a carrier
/// that satisfies `holds`, so that a read of `topic` by either node sees it.
///
/// It reads only the owners' storage, never the DHT: a probe that fetched would wake itself on
/// its own lookup and, answered locally, never yield to the deliveries it waits for.
async fn await_owners_holding(
    nodes: [&Processor; 2],
    topic: &str,
    holds: impl Fn(&Entry) -> bool,
) -> Result<()> {
    let placements = Entry::gen_did(topic)?.rotate_affine(DATA_REDUNDANT)?;
    let holds = &holds;
    probe_on_activity(
        "every owner of the topic holds the writes",
        PROCESSOR_TEST_HANG_GUARD,
        || {
            let placements = &placements;
            async move {
                for placement in placements {
                    let mut owners = nodes
                        .iter()
                        .filter(|node| owns_entry_placement(node, *placement).unwrap_or(false));
                    let Some(owner) = owners.next() else {
                        return Ok(None);
                    };
                    let stored = owner
                        .swarm
                        .dht()
                        .stored_entry_at(EntryKind::Data, *placement, get_epoch_ms())
                        .await
                        .map_err(Error::Storage)?;
                    if !stored.is_some_and(|entry| holds(&entry)) {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            }
        },
    )
    .await
}

/// Whether a carrier holds every one of `elements`.
fn holds_all(elements: &[Bytes]) -> impl Fn(&Entry) -> bool + '_ {
    |entry| elements.iter().all(|element| entry.data.contains(element))
}

/// Publish `recipient`'s mail descriptor once, await its owners holding it, and return what the
/// gateway then finds.
async fn opt_in(
    gateway: &MailGateway,
    nodes: [&Processor; 2],
    recipient: &Processor,
) -> Result<MailRecipient> {
    MailRegistration::new(recipient.did(), MailOffer::default())
        .register_once(&RegistrationContext::new(recipient))
        .await?;
    await_owners_holding(nodes, &descriptor_topic(recipient.did()), |entry| {
        !entry.data.is_empty()
    })
    .await?;
    Ok(gateway
        .recipient(recipient.did())
        .await
        .unwrap()
        .expect("the gateway finds the descriptor its owners hold"))
}

/// The recipient of `processor`, its inbox and records empty.
fn mem_recipient(processor: &Arc<Processor>) -> MemRecipient {
    Recipient::start(
        Arc::clone(processor),
        KvInbox::new(MemStorage::new()),
        MemStorage::new(),
        DEFAULT_MAIL_POLL_INTERVAL,
    )
    .unwrap()
}

/// [`MESSAGE`] sealed for `target`.
fn sealed_for(target: &MailRecipient) -> SealedMail {
    seal(MESSAGE, target, &mut OsRng).unwrap()
}

/// The messages `recipient`'s inbox holds.
async fn inbox_messages(recipient: &MemRecipient) -> Vec<Vec<u8>> {
    use rings_core::storage::KvStorageInterface;
    recipient
        .inbox()
        .store()
        .get_all()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, stored)| stored.message)
        .collect()
}

#[tokio::test]
async fn test_a_mail_through_the_gateway_is_opened_by_its_recipient_within_its_lifetime(
) -> Result<()> {
    let (_network, gateway_node, recipient_node) = connected_pair().await?;
    let nodes = [gateway_node.as_ref(), recipient_node.as_ref()];
    let gateway = MailGateway::new(Arc::clone(&gateway_node));
    let target = opt_in(&gateway, nodes, &recipient_node).await?;
    assert_eq!(
        target.encryption_key,
        recipient_node.delegatee_key().delegatee_public_key()
    );
    // Sent twice, as to `d@a` and `d@b`: two ciphertexts of one `Message-ID`.
    gateway.deliver(&target, MESSAGE).await.unwrap();
    gateway.deliver(&target, MESSAGE).await.unwrap();
    // Two mails of two elements each.
    await_owners_holding(nodes, &mail_topic(target.did), |entry| {
        entry.data.len() == 4
    })
    .await?;

    let recipient = mem_recipient(&recipient_node);
    assert_eq!(
        recipient.poll().await.unwrap(),
        PollReport {
            filed: 1,
            duplicates: 1,
            ..PollReport::default()
        },
        "deduplicated by Message-ID"
    );
    assert_eq!(inbox_messages(&recipient).await, vec![MESSAGE.to_vec()]);
    assert_eq!(
        recipient.poll().await.unwrap(),
        PollReport::default(),
        "an opened mail is not opened again"
    );
    Ok(())
}

#[tokio::test]
async fn test_rcpt_for_a_did_without_a_live_descriptor_finds_no_recipient() -> Result<()> {
    let (_network, gateway_node, recipient_node) = connected_pair().await?;
    let gateway = MailGateway::new(Arc::clone(&gateway_node));
    assert_eq!(gateway.recipient(recipient_node.did()).await.unwrap(), None);
    assert_eq!(
        gateway.recipient(gateway_node.did()).await.unwrap(),
        None,
        "a node that does not opt in takes no mail, the gateway's own included"
    );
    Ok(())
}

#[tokio::test]
async fn test_a_forged_chunk_on_the_topic_is_ignored() -> Result<()> {
    let (_network, gateway_node, recipient_node) = connected_pair().await?;
    let nodes = [gateway_node.as_ref(), recipient_node.as_ref()];
    let gateway = MailGateway::new(Arc::clone(&gateway_node));
    let target = opt_in(&gateway, nodes, &recipient_node).await?;
    let sealed = sealed_for(&target);
    let topic = mail_topic(target.did);
    gateway_node
        .swarm
        .storage_extend(&topic, sealed.elements.clone(), MAX_TTL_MS)
        .await?;
    let forged = MailElement::Chunk(Bytes::from_static(b"forged"))
        .encode()
        .unwrap();
    gateway_node
        .storage_append_data(&topic, forged.clone())
        .await?;
    let written = [sealed.elements, vec![forged]].concat();
    await_owners_holding(nodes, &topic, holds_all(&written)).await?;

    let recipient = mem_recipient(&recipient_node);
    let report = recipient.poll().await.unwrap();
    assert_eq!(report, PollReport {
        filed: 1,
        ..PollReport::default()
    });
    assert_eq!(inbox_messages(&recipient).await, vec![MESSAGE.to_vec()]);
    Ok(())
}

#[tokio::test]
async fn test_a_missing_chunk_on_the_topic_discards_the_mail() -> Result<()> {
    let (_network, gateway_node, recipient_node) = connected_pair().await?;
    let nodes = [gateway_node.as_ref(), recipient_node.as_ref()];
    let gateway = MailGateway::new(Arc::clone(&gateway_node));
    let target = opt_in(&gateway, nodes, &recipient_node).await?;
    let mut sealed = sealed_for(&target);
    sealed.elements.pop();
    let topic = mail_topic(target.did);
    gateway_node
        .swarm
        .storage_extend(&topic, sealed.elements.clone(), MAX_TTL_MS)
        .await?;
    await_owners_holding(nodes, &topic, holds_all(&sealed.elements)).await?;

    let recipient = mem_recipient(&recipient_node);
    let report = recipient.poll().await.unwrap();
    assert_eq!(report, PollReport {
        incomplete: 1,
        ..PollReport::default()
    });
    assert!(inbox_messages(&recipient).await.is_empty());
    Ok(())
}

/// Law (lifetime): every node projects a carrier at its clock (`Entry::retired_at`), so the mail
/// a recipient reads opens until `τ + MAX_TTL_MS` and is gone from that instant on.
#[tokio::test]
async fn test_a_mail_not_fetched_within_its_lifetime_is_gone() -> Result<()> {
    let (_network, gateway_node, recipient_node) = connected_pair().await?;
    let nodes = [gateway_node.as_ref(), recipient_node.as_ref()];
    let gateway = MailGateway::new(Arc::clone(&gateway_node));
    let target = opt_in(&gateway, nodes, &recipient_node).await?;
    gateway.deliver(&target, MESSAGE).await.unwrap();
    let topic = mail_topic(target.did);
    await_owners_holding(nodes, &topic, |entry| entry.data.len() == 2).await?;

    let entry = recipient_node
        .fetch_storage_entry(Entry::gen_did(&topic)?)
        .await?
        .expect("the recipient reads the mail its owners hold");
    let issued_at = entry
        .crdt
        .dots
        .iter()
        .map(|dot| dot.version.logical_time_ms)
        .min()
        .expect("the written mail has dots");
    let key = recipient_node.delegatee_key();
    let opened_at = |now_ms: u128| {
        let elements = entry.clone().retired_at(now_ms).data;
        open_mail(&elements, target.did, key, |_| true)
    };
    let last_instant = issued_at + u128::from(MAX_TTL_MS) - 1;
    assert!(matches!(
        opened_at(last_instant).as_slice(),
        [Opening::Opened(mail)] if mail.message == MESSAGE
    ));
    assert_eq!(opened_at(last_instant + 1), vec![]);
    Ok(())
}
