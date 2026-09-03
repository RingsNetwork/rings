use super::inbox::inbox_destination;
use super::inbox::inbox_key;
use super::Entry;
use super::EntryKind;
use super::EntryOperation;
use crate::consts::DEFAULT_RELAY_INBOX_TTL_MS;
use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::session::SessionSk;
use crate::tests::TEST_NETWORK_ID;

const NOW_MS: u128 = 1_700_000_000_000;

fn session() -> Result<SessionSk> {
    SessionSk::new_with_seckey(&SecretKey::random())
}

fn custom_to(destination: Did, network_id: u32) -> Result<MessagePayload> {
    let session = session()?;
    MessagePayload::new_send(
        Message::custom(b"held")?,
        MessageSigner::new(&session, network_id),
        destination,
        destination,
    )
}

fn live(mut entry: Entry) -> Entry {
    entry.expires_at_ms = Some(NOW_MS + 1_000);
    entry
}

/// Law: `inbox_destination ∘ inbox_key = id`, and the inbox of `d` is the position after `d`.
#[test]
fn test_inbox_key_is_the_position_after_the_destination() {
    let destination = Did::from(41u32);
    assert_eq!(inbox_key(destination), Did::from(42u32));
    assert_eq!(inbox_destination(inbox_key(destination)), destination);
    assert_eq!(inbox_destination(Did::from(0u32)), -Did::from(1u32));
}

/// Witness: a held message is admissible iff it is addressed to the inbox owner, carries an
/// application message, and both signatures verify inside the receiver's overlay.
#[test]
fn test_inbox_admits_only_messages_addressed_to_its_owner() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let held = live(Entry::inbox_delta(&custom_to(
        destination,
        TEST_NETWORK_ID,
    )?)?);
    assert_eq!(held.did, inbox_key(destination));
    assert_eq!(held.kind, EntryKind::RelayMessage);
    held.validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
    assert_eq!(
        held.deliverable_inbox_messages(TEST_NETWORK_ID)
            .into_iter()
            .map(|payload| payload.transaction.destination)
            .collect::<Vec<_>>(),
        vec![destination]
    );

    let misfiled = live(Entry::new(
        held.did,
        Entry::inbox_delta(&custom_to(
            SecretKey::random().address().into(),
            TEST_NETWORK_ID,
        )?)?
        .data,
        EntryKind::RelayMessage,
    ));
    assert!(matches!(
        misfiled.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::RelayMessageNotAddressedToInbox)
    ));
    assert!(misfiled
        .deliverable_inbox_messages(TEST_NETWORK_ID)
        .is_empty());
    Ok(())
}

/// Witness: a message signed inside another overlay is not admitted, even when addressed
/// correctly.
#[test]
fn test_inbox_rejects_messages_signed_in_another_overlay() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let foreign = live(Entry::inbox_delta(&custom_to(
        destination,
        TEST_NETWORK_ID + 1,
    )?)?);
    assert!(matches!(
        foreign.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::RelayMessageUnverifiable)
    ));
    Ok(())
}

/// Witness: only application messages may be held; a control message is refused.
#[test]
fn test_inbox_rejects_control_messages() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let session = session()?;
    let control = MessagePayload::new_send(
        Message::PeerLivenessProbe(crate::message::PeerLivenessProbe { sent_at_ms: 1 }),
        MessageSigner::new(&session, TEST_NETWORK_ID),
        destination,
        destination,
    )?;
    let held = live(Entry::inbox_delta(&control)?);
    assert!(matches!(
        held.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::RelayMessageNotApplication)
    ));
    Ok(())
}

/// Witness: an element that is not a payload at all is refused.
#[test]
fn test_inbox_rejects_undecodable_elements() {
    let held = live(Entry::new(
        inbox_key(Did::from(7u32)),
        vec![crate::message::Encoded::from("not a payload")],
        EntryKind::RelayMessage,
    ));
    assert!(held
        .validate_admissible_at(NOW_MS, TEST_NETWORK_ID)
        .is_err());
}

/// Retention policy is a property of the kind: a relay inbox is stamped with and admitted up to
/// its own, longer, bounds, while a data topic keeps the message bounds.
#[test]
fn test_inbox_uses_its_own_retention_policy() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let delta = Entry::inbox_delta(&custom_to(destination, TEST_NETWORK_ID)?)?;
    let stamped = EntryOperation::Extend(delta.clone()).stamped(NOW_MS, Did::from(1u32))?;
    assert_eq!(
        stamped.entry().expires_at_ms,
        Some(NOW_MS + u128::from(DEFAULT_RELAY_INBOX_TTL_MS))
    );
    assert!(u128::from(DEFAULT_RELAY_INBOX_TTL_MS) > u128::from(MAX_TTL_MS));

    let mut at_limit = delta.clone();
    at_limit.expires_at_ms =
        Some(NOW_MS + u128::from(MAX_RELAY_INBOX_TTL_MS) + TS_OFFSET_TOLERANCE_MS);
    at_limit.validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;

    let mut beyond = delta;
    beyond.expires_at_ms =
        Some(NOW_MS + u128::from(MAX_RELAY_INBOX_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1);
    assert!(matches!(
        beyond.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryLifetimeExceedsMax)
    ));
    Ok(())
}

/// Draining: compacting the delivered messages out of the inbox leaves a floor that excludes
/// them from every later join, so a stale copy cannot redeliver.
#[test]
fn test_compaction_retires_delivered_messages_from_the_inbox() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let owner = Did::from(9u32);
    let first = Entry::inbox_delta(&custom_to(destination, TEST_NETWORK_ID)?)?;
    let second = Entry::inbox_delta(&custom_to(destination, TEST_NETWORK_ID)?)?;
    let inbox = Entry::new(first.did, Vec::new(), EntryKind::RelayMessage)
        .operate(
            NOW_MS,
            EntryOperation::Extend(first.clone()).stamped(NOW_MS, owner)?,
            owner,
        )?
        .operate(
            NOW_MS,
            EntryOperation::Extend(second.clone()).stamped(NOW_MS, owner)?,
            owner,
        )?;
    assert_eq!(inbox.data.len(), 2);

    let delivered = Entry::new(first.did, first.data.clone(), EntryKind::RelayMessage);
    let compaction = EntryOperation::CompactData(delivered).stamped(NOW_MS + 1, destination)?;
    let compacted = inbox.operate(NOW_MS + 1, compaction, destination)?;
    assert_eq!(compacted.data, second.data);
    assert!(compacted.crdt.tombstones.is_empty());

    let stale_copy = compacted.join(inbox)?;
    assert_eq!(stale_copy.data, second.data);
    Ok(())
}
