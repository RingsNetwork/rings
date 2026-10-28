//! The pure laws of mail: addressing, the descriptor's key, the envelope's sizes, the
//! opening of a topic, and the gateway's verdict on a written mail.

use std::future::pending;
use std::future::ready;
use std::time::Duration;

use bytes::Bytes;
use futures::executor::block_on;
use futures::FutureExt;
use rand::rngs::OsRng;
use rings_core::consts::ENTRY_PAYLOAD_MAX_BYTES;
use rings_core::delegation::DelegateeKey;
use rings_core::dht::Did;
use rings_core::ecc::SecretKey;
use rings_core::lifecycle::StopSource;
use rings_core::message::MessageSigner;
use rings_core::swarm::storage_api::StoreUnconfirmed;
use rings_core::utils::get_epoch_ms;

use super::address::addressed_did;
use super::descriptor::MailDescriptor;
use super::descriptor::MailDescriptorBody;
use super::descriptor::MailRecipient;
use super::envelope::seal;
use super::envelope::MailDigest;
use super::envelope::MailElement;
use super::envelope::MailManifest;
use super::envelope::SealError;
use super::envelope::SealedMail;
use super::envelope::MAIL_CHUNKS_MAX;
use super::envelope::MAIL_CHUNK_SLICE_MAX;
use super::envelope::MAIL_CIPHERTEXT_MAX;
use super::envelope::MAIL_MESSAGE_MAX;
use super::gateway::confirmed;
use super::gateway::stopping_after;
use super::gateway::MailGatewayError;
use super::open::open_mail;
use super::open::OpenedMail;
use super::open::Opening;
use super::recipient::RecipientRecord;
use super::recipient::MESSAGE_ID_RETENTION_MS;
use super::recipient::OPENED_RETENTION_MS;
use super::registration::MailOffer;
use crate::descriptor::SignedDescriptor;

/// The overlay every fixture is signed for.
const NETWORK: u32 = 7;
/// A message with a `Message-ID`.
const MESSAGE: &[u8] =
    b"Message-ID: <hello@example.org>\r\nSubject: hello\r\n\r\nAcross the Great Wall.\r\n";

/// A fresh delegatee key of a fresh account.
fn key() -> DelegateeKey {
    DelegateeKey::new_with_seckey(&SecretKey::random()).unwrap()
}

/// Where mail sealed for `key`'s account goes.
fn recipient_of(key: &DelegateeKey) -> MailRecipient {
    MailRecipient {
        did: key.delegator_did(),
        encryption_key: key.delegatee_public_key(),
    }
}

/// `message` sealed for `recipient`.
fn sealed(message: &[u8], recipient: &DelegateeKey) -> SealedMail {
    seal(message, &recipient_of(recipient), &mut OsRng).unwrap()
}

/// What `elements` open to for `recipient` under `key`, nothing skipped.
fn opened(elements: &[Bytes], recipient: Did, key: &DelegateeKey) -> Vec<Opening> {
    open_mail(elements, recipient, key, |_| true)
}

/// The messages among `openings`.
fn messages(openings: &[Opening]) -> Vec<&[u8]> {
    openings
        .iter()
        .filter_map(|opening| match opening {
            Opening::Opened(mail) => Some(mail.message.as_slice()),
            Opening::Incomplete { .. } | Opening::Unreadable { .. } => None,
        })
        .collect()
}

#[test]
fn test_addressed_did_reads_the_canonical_did_in_any_case_and_domain() {
    let did = key().delegator_did();
    let upper = format!(
        "0x{}",
        did.to_string().trim_start_matches("0x").to_uppercase()
    );
    for (address, expected) in [
        (format!("{did}@p2p.rings.rs"), Some(did)),
        (format!("{did}@another.example"), Some(did)),
        (format!("{upper}@p2p.rings.rs"), Some(did)),
        (format!("{did}@"), None),
        (did.to_string(), None),
        (
            format!("{}@p2p.rings.rs", did.to_string().trim_start_matches("0x")),
            None,
        ),
        ("me@p2p.rings.rs".to_owned(), None),
        (format!("\"{did}\"@p2p.rings.rs"), None),
    ] {
        assert_eq!(addressed_did(&address), expected, "{address}");
    }
}

/// The key law: mail to a descriptor is sealed to the key its signature recovers to, the
/// signing delegatee's; a descriptor altered after signing, read in another overlay or read for
/// another DID gives no key.
#[test]
fn test_a_descriptor_seals_mail_to_the_key_that_signed_it() {
    let key = key();
    let now_ms = get_epoch_ms();
    let body = MailDescriptorBody {
        did: key.delegator_did(),
        public_key: key.delegation().delegator_verification_pubkey().unwrap(),
        network_id: NETWORK,
        fresh_for_ms: 90_000,
        expires_at_ms: now_ms + 90_000,
    };
    let descriptor = MailDescriptor::new_signed(body, MessageSigner::new(&key, NETWORK))
        .unwrap()
        .heard_at(now_ms);
    let did = key.delegator_did();
    let live = |descriptor: &MailDescriptor, did, network| {
        MailDescriptor::live_recipient([descriptor.clone()], did, now_ms, network)
    };
    assert_eq!(live(&descriptor, did, NETWORK), Some(recipient_of(&key)));
    let mut altered = descriptor.clone();
    altered.body.fresh_for_ms -= 1;
    assert_eq!(live(&altered, did, NETWORK), None);
    assert_eq!(live(&descriptor, did, NETWORK + 1), None);
    assert_eq!(
        live(&descriptor, Did::from(7u32), NETWORK),
        None,
        "a descriptor speaks only for its own DID"
    );
}

#[test]
fn test_the_default_mail_offer_satisfies_the_timing_law() {
    let offer = MailOffer::default();
    assert_eq!(
        MailOffer::new(offer.heartbeat_interval(), offer.ttl()).unwrap(),
        offer
    );
    assert!(MailOffer::new(offer.ttl(), offer.heartbeat_interval()).is_err());
}

#[test]
fn test_the_widest_chunk_element_is_one_entry_payload() {
    let chunk = MailElement::Chunk(Bytes::from(vec![0xff; MAIL_CHUNK_SLICE_MAX]));
    assert_eq!(chunk.encode().unwrap().len(), ENTRY_PAYLOAD_MAX_BYTES);
}

#[test]
fn test_the_largest_message_seals_into_the_most_chunks() {
    let recipient = key();
    let largest = sealed(&vec![b'x'; MAIL_MESSAGE_MAX], &recipient);
    assert_eq!(largest.elements.len(), 1 + MAIL_CHUNKS_MAX);
    assert!(largest
        .elements
        .iter()
        .all(|element| element.len() <= ENTRY_PAYLOAD_MAX_BYTES));
    let ciphertext = largest
        .elements
        .iter()
        .skip(1)
        .map(|element| match MailElement::decode(element).unwrap() {
            MailElement::Chunk(slice) => slice.len(),
            MailElement::Manifest(_) => panic!("only the first element is the manifest"),
        })
        .sum::<usize>();
    assert_eq!(ciphertext, MAIL_CIPHERTEXT_MAX);

    let too_large = seal(
        &vec![b'x'; MAIL_MESSAGE_MAX + 1],
        &recipient_of(&recipient),
        &mut OsRng,
    );
    assert!(matches!(
        too_large,
        Err(SealError::TooLarge { chunks }) if chunks == MAIL_CHUNKS_MAX + 1
    ));
}

#[test]
fn test_a_sealed_mail_opens_only_for_its_recipient() {
    let recipient = key();
    let mail = sealed(MESSAGE, &recipient);
    let did = recipient.delegator_did();
    assert_eq!(opened(&mail.elements, did, &recipient), vec![
        Opening::Opened(OpenedMail {
            mail_id: mail.mail_id,
            message: MESSAGE.to_vec(),
        })
    ]);
    // Law (round trip): the elements' order is immaterial.
    let reversed = mail.elements.iter().rev().cloned().collect::<Vec<_>>();
    assert_eq!(messages(&opened(&reversed, did, &recipient)), vec![MESSAGE]);
    assert_eq!(
        opened(&mail.elements, did, &key()),
        vec![Opening::Unreadable {
            mail_id: mail.mail_id
        }],
        "another key opens nothing"
    );
    assert_eq!(
        opened(&mail.elements, key().delegator_did(), &recipient),
        vec![Opening::Unreadable {
            mail_id: mail.mail_id
        }],
        "the associated data binds the DID: under another, even its key opens nothing"
    );
}

/// The integrity law: a chunk no manifest lists is never read, and in place of a listed one it
/// completes nothing.
#[test]
fn test_a_forged_chunk_is_ignored() {
    let recipient = key();
    let mail = sealed(MESSAGE, &recipient);
    let forged = MailElement::Chunk(Bytes::from_static(b"forged"))
        .encode()
        .unwrap();
    let did = recipient.delegator_did();
    let with_forged = std::iter::once(forged.clone())
        .chain(mail.elements.iter().cloned())
        .collect::<Vec<_>>();
    assert_eq!(messages(&opened(&with_forged, did, &recipient)), vec![
        MESSAGE
    ]);
    // In place of the genuine chunk, the forged one completes nothing.
    let [manifest, ..] = mail.elements.as_slice() else {
        panic!("a sealed mail has a manifest");
    };
    assert_eq!(opened(&[manifest.clone(), forged], did, &recipient), vec![
        Opening::Incomplete {
            mail_id: mail.mail_id
        }
    ]);
}

#[test]
fn test_a_missing_chunk_discards_the_mail() {
    let recipient = key();
    let mail = sealed(&vec![b'x'; 3 * MAIL_CHUNK_SLICE_MAX], &recipient);
    assert_eq!(mail.elements.len(), 1 + 4);
    let did = recipient.delegator_did();
    for missing in 1..mail.elements.len() {
        let elements = mail
            .elements
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != missing)
            .map(|(_, element)| element.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            opened(&elements, did, &recipient),
            vec![Opening::Incomplete {
                mail_id: mail.mail_id
            }],
            "chunk {missing} missing"
        );
    }
}

/// Law (content address): a manifest anyone writes for the genuine mail id, over chunks that
/// do not concatenate to its preimage, is ignored, so it can neither replace the genuine mail
/// nor mark it unreadable while its chunks are missing.
#[test]
fn test_a_forged_manifest_never_hides_the_genuine_mail() {
    let recipient = key();
    let mail = sealed(MESSAGE, &recipient);
    let did = recipient.delegator_did();
    let forged_chunk = MailElement::Chunk(Bytes::from_static(b"forged"))
        .encode()
        .unwrap();
    let forged_manifest = MailManifest {
        mail_id: mail.mail_id,
        chunks: vec![MailDigest::of(&forged_chunk)],
    };
    let forgery = vec![
        MailElement::Manifest(forged_manifest).encode().unwrap(),
        forged_chunk,
    ];
    for elements in [
        [forgery.clone(), mail.elements.clone()].concat(),
        [mail.elements.clone(), forgery.clone()].concat(),
    ] {
        assert_eq!(messages(&opened(&elements, did, &recipient)), vec![MESSAGE]);
    }
    let genuine_manifest = mail.elements.first().cloned().unwrap();
    assert_eq!(
        opened(&[forgery, vec![genuine_manifest]].concat(), did, &recipient),
        vec![Opening::Incomplete {
            mail_id: mail.mail_id
        }]
    );
}

#[test]
fn test_message_id_is_read_from_the_header_only() {
    let mail = |message: &[u8]| OpenedMail {
        mail_id: MailDigest::of(message),
        message: message.to_vec(),
    };
    for (message, expected) in [
        (MESSAGE, Some("<hello@example.org>")),
        (b"message-id:  <a@b>  \r\n\r\n".as_slice(), Some("<a@b>")),
        (
            b"Message-ID:\r\n <folded@b>\r\nSubject: x\r\n\r\n",
            Some("<folded@b>"),
        ),
        (b"Subject: x\r\n\r\nMessage-ID: <body@b>\r\n", None),
        (b"Subject: x\n X-Folded: Message-ID: <no@b>\n\n", None),
        (b"Message-ID:\r\n\r\n", None),
    ] {
        assert_eq!(
            mail(message).message_id().as_deref(),
            expected,
            "{}",
            String::from_utf8_lossy(message)
        );
    }
}

#[test]
fn test_records_outlive_their_retention_exactly() {
    for (record, retention) in [
        (
            RecipientRecord::Opened { at_ms: 1_000 },
            OPENED_RETENTION_MS,
        ),
        (
            RecipientRecord::Filed { at_ms: 1_000 },
            MESSAGE_ID_RETENTION_MS,
        ),
    ] {
        assert!(!record.outlived_at(1_000 + retention - 1), "{record:?}");
        assert!(record.outlived_at(1_000 + retention), "{record:?}");
    }
}

/// The one-key law: mail sealed to another delegatee key of the recipient's account, one a
/// browser node held before its last load, does not open under the current key.
#[test]
fn test_mail_sealed_to_a_key_the_recipient_no_longer_holds_never_opens() {
    let account = SecretKey::random();
    let before = DelegateeKey::new_with_seckey(&account).unwrap();
    let current = DelegateeKey::new_with_seckey(&account).unwrap();
    assert_eq!(before.delegator_did(), current.delegator_did());
    let mail = sealed(MESSAGE, &before);
    assert_eq!(
        opened(&mail.elements, current.delegator_did(), &current),
        vec![Opening::Unreadable {
            mail_id: mail.mail_id
        }]
    );
}

/// The DATA verdict (`gateway::confirmed`): `250` iff the receipt resolves stored before the
/// wait elapses, a report at the deadline included; `451` when every owner refused or the wait
/// elapsed first.
#[test]
fn test_a_mail_is_confirmed_only_by_a_stored_receipt_within_the_wait() {
    let did = Did::from(1_u32);
    let refused = StoreUnconfirmed::Refused {
        placement: Did::from(2_u32),
    };
    let verdict = |receipt: Option<Result<(), StoreUnconfirmed>>, elapsed: bool| {
        let receipt = match receipt {
            Some(resolved) => ready(resolved).left_future(),
            None => pending().right_future(),
        };
        let elapsed = match elapsed {
            true => ready(()).left_future(),
            false => pending().right_future(),
        };
        confirmed(did, receipt, elapsed).now_or_never()
    };

    assert!(matches!(verdict(Some(Ok(())), false), Some(Ok(()))));
    assert!(
        matches!(verdict(Some(Ok(())), true), Some(Ok(()))),
        "a report at the deadline counts"
    );
    assert!(matches!(
        verdict(Some(Err(refused)), false),
        Some(Err(MailGatewayError::Unstored { cause, .. })) if cause == refused
    ));
    assert!(matches!(
        verdict(None, true),
        Some(Err(MailGatewayError::Unreported { did: unreported })) if unreported == did
    ));
    assert!(verdict(None, false).is_none(), "the verdict waits");
}

/// The deadline (`gateway::stopping_after`): a write waiting on nothing but the stop ends once
/// the wait has elapsed, with the stop requested; a write that ends first is never stopped.
#[test]
fn test_a_write_ends_at_its_deadline_stop() {
    let deadline = StopSource::new();
    let stop = deadline.token();
    let ended = block_on(stopping_after(Duration::ZERO, &deadline, async {
        stop.stopped().await;
        "stopped"
    }));
    assert_eq!(ended, "stopped");
    assert!(deadline.is_stop_requested());

    let deadline = StopSource::new();
    let ended = block_on(stopping_after(
        Duration::from_secs(3600),
        &deadline,
        ready(1),
    ));
    assert_eq!(ended, 1);
    assert!(!deadline.is_stop_requested());
}
