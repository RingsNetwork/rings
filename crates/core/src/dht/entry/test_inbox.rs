use super::inbox::inbox_destination;
use super::inbox::inbox_key;
use super::inbox::validate_inbox_relocation;
use super::inbox::HeldMessage;
use super::inbox::HELD_MESSAGE_DOMAIN_TAG;
use super::Entry;
use super::EntryKind;
use super::EntryOperation;
use super::EntryVersion;
use crate::consts::DEFAULT_RELAY_INBOX_TTL_MS;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::MessageVerification;
use crate::message::NotifyPredecessorSend;
use crate::message::SigningDomain;
use crate::session::SessionSk;
use crate::tests::with_retention;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

fn session() -> Result<SessionSk> {
    SessionSk::new_with_seckey(&SecretKey::random())
}

fn payload_to(message: Message, destination: Did, network_id: u32) -> Result<MessagePayload> {
    let sender = session()?;
    MessagePayload::new_send(
        message,
        MessageSigner::new(&sender, network_id),
        destination,
        destination,
    )
}

fn custom_to(destination: Did, network_id: u32) -> Result<MessagePayload> {
    payload_to(Message::custom(b"held")?, destination, network_id)
}

/// A hold by `holder` of a fresh custom message to `destination`, inside `network_id`.
fn held_by(holder: &SessionSk, destination: Did, network_id: u32) -> Result<HeldMessage> {
    HeldMessage::hold(
        custom_to(destination, network_id)?,
        MessageSigner::new(holder, network_id),
    )
}

/// A holder signature stamped at an arbitrary instant, for laws about the hold instant.
fn holder_signature_at(
    holder: &SessionSk,
    payload: &MessagePayload,
    ts_ms: u128,
) -> Result<MessageVerification> {
    let data = rings_codec::serialize(payload).map_err(Error::CodecSerialize)?;
    let domain = SigningDomain::new(HELD_MESSAGE_DOMAIN_TAG, TEST_NETWORK_ID);
    let msg = domain.transcript(&data, ts_ms, DEFAULT_TTL_MS);
    Ok(MessageVerification {
        session: holder.session(),
        sig: holder.sign(&msg)?,
        ttl_ms: DEFAULT_TTL_MS,
        ts_ms,
    })
}

/// The owner's carrier after admitting `delta` as a hold at `now_ms`.
fn carrier_with(delta: Entry, now_ms: u128, actor: Did) -> Result<Entry> {
    Entry::new(delta.did, Vec::new(), EntryKind::RelayMessage).extend(now_ms, delta, actor)
}

#[test]
fn test_inbox_key_is_the_position_after_the_destination() {
    let destination = Did::from(41u32);
    assert_eq!(inbox_key(destination), Did::from(42u32));
    assert_eq!(inbox_destination(inbox_key(destination)), destination);
}

#[test]
fn test_witness_admits_a_held_custom_message_addressed_to_the_recipient() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let delta = with_retention(
        Entry::inbox_delta(&held)?,
        now_ms + u128::from(DEFAULT_RELAY_INBOX_TTL_MS),
    );

    assert_eq!(delta.did, inbox_key(destination));
    delta.validate_admissible_at(now_ms, TEST_NETWORK_ID)?;
    assert_eq!(held.holder(), holder.account_did());
    Ok(())
}

#[test]
fn test_witness_rejects_a_message_addressed_elsewhere() -> Result<()> {
    let now_ms = get_epoch_ms();
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&session()?, destination, TEST_NETWORK_ID)?;
    let mut misfiled = with_retention(
        Entry::inbox_delta(&held)?,
        now_ms + u128::from(DEFAULT_RELAY_INBOX_TTL_MS),
    );
    misfiled.did = inbox_key(SecretKey::random().address().into());

    assert!(matches!(
        misfiled.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageNotAddressedToInbox)
    ));
    Ok(())
}

#[test]
fn test_witness_rejects_every_message_but_custom() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let control = payload_to(
        Message::NotifyPredecessorSend(NotifyPredecessorSend { did: destination }),
        destination,
        TEST_NETWORK_ID,
    )?;
    let held = HeldMessage::hold(control, MessageSigner::new(&holder, TEST_NETWORK_ID))?;

    assert!(matches!(
        held.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageNotCustom)
    ));
    Ok(())
}

#[test]
fn test_witness_binds_holder_and_payload_to_the_overlay() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();

    let foreign_hold = HeldMessage::hold(
        custom_to(destination, TEST_NETWORK_ID)?,
        MessageSigner::new(&holder, TEST_NETWORK_ID + 1),
    )?;
    assert!(matches!(
        foreign_hold.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageUnverifiable)
    ));

    let foreign_payload = HeldMessage::hold(
        custom_to(destination, TEST_NETWORK_ID + 1)?,
        MessageSigner::new(&holder, TEST_NETWORK_ID),
    )?;
    assert!(matches!(
        foreign_payload.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageHeldOutsideSenderProof)
    ));
    Ok(())
}

#[test]
fn test_witness_rejects_a_hold_ahead_of_the_clock() -> Result<()> {
    let now_ms = get_epoch_ms();
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&session()?, destination, TEST_NETWORK_ID)?;

    let behind = now_ms - TS_OFFSET_TOLERANCE_MS - 1;
    assert!(matches!(
        held.validate_witness(destination, behind, TEST_NETWORK_ID),
        Err(Error::RelayMessageHeldAheadOfClock)
    ));
    Ok(())
}

#[test]
fn test_witness_judges_the_payload_as_of_the_hold_instant() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let payload = custom_to(destination, TEST_NETWORK_ID)?;

    // A hold after the sender's proof lifetime is not a hold of a live message.
    let late = get_epoch_ms() + u128::from(DEFAULT_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1;
    let held_late = HeldMessage {
        holder: holder_signature_at(&holder, &payload, late)?,
        payload: payload.clone(),
    };
    assert!(matches!(
        held_late.validate_witness(destination, late, TEST_NETWORK_ID),
        Err(Error::RelayMessageHeldOutsideSenderProof)
    ));

    // A hold inside it stays admissible however far the receiver's clock has moved on: the
    // witness is monotone in `now_ms`.
    let held = HeldMessage::hold(payload, MessageSigner::new(&holder, TEST_NETWORK_ID))?;
    let far_future = get_epoch_ms() + u128::from(MAX_RELAY_INBOX_TTL_MS);
    held.validate_witness(destination, far_future, TEST_NETWORK_ID)?;
    Ok(())
}

#[test]
fn test_witness_rejects_a_tampered_payload_and_a_reset_floor() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let mut held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    held.payload.transaction.tx_id = uuid::Uuid::new_v4();
    assert!(matches!(
        held.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageUnverifiable)
    ));

    let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let mut with_floor = with_retention(
        Entry::inbox_delta(&held)?,
        now_ms + u128::from(DEFAULT_RELAY_INBOX_TTL_MS),
    );
    with_floor.crdt.register = Some(EntryVersion::new(
        now_ms,
        holder.account_did(),
        Did::from(0u32),
    ));
    assert!(matches!(
        with_floor.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::RelayInboxRegisterNotAllowed)
    ));
    Ok(())
}

#[test]
fn test_inbox_uses_its_own_retention_policy() -> Result<()> {
    let now_ms = get_epoch_ms();
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&session()?, destination, TEST_NETWORK_ID)?;
    let delta = Entry::inbox_delta(&held)?;

    let stamped = EntryOperation::Extend(delta.clone()).stamped(now_ms, Did::from(9u32))?;
    assert_eq!(
        stamped.entry().expires_at_ms,
        Some(now_ms + u128::from(DEFAULT_RELAY_INBOX_TTL_MS))
    );

    let mut long = delta.clone();
    long.expires_at_ms = Some(now_ms + u128::from(MAX_RELAY_INBOX_TTL_MS));
    long.validate_admissible_at(now_ms, TEST_NETWORK_ID)?;

    let mut too_long = delta;
    too_long.expires_at_ms =
        Some(now_ms + u128::from(MAX_RELAY_INBOX_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1);
    assert!(matches!(
        too_long.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::EntryLifetimeExceedsMax)
    ));
    assert!(u128::from(MAX_TTL_MS) < u128::from(MAX_RELAY_INBOX_TTL_MS));
    Ok(())
}

#[test]
fn test_hold_authority_admits_only_the_responsible_node() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let hold = EntryOperation::Extend(Entry::inbox_delta(&held)?);
    let responsible = holder.account_did();
    let stranger: Did = SecretKey::random().address().into();

    hold.validate_inbox_authority(responsible, Some(responsible))?;
    assert!(matches!(
        hold.validate_inbox_authority(stranger, Some(responsible)),
        Err(Error::RelayMessageHolderNotResponsible)
    ));
    assert!(matches!(
        hold.validate_inbox_authority(responsible, Some(stranger)),
        Err(Error::RelayMessageHolderNotResponsible)
    ));
    assert!(matches!(
        hold.validate_inbox_authority(responsible, None),
        Err(Error::RelayMessageHolderNotResponsible)
    ));
    Ok(())
}

#[test]
fn test_removal_authority_is_the_recipient_alone_and_nothing_else_is_allowed() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage);
    let responsible: Did = SecretKey::random().address().into();

    EntryOperation::Tombstone(carrier.clone())
        .validate_inbox_authority(destination, Some(responsible))?;
    assert!(matches!(
        EntryOperation::Tombstone(carrier.clone())
            .validate_inbox_authority(responsible, Some(responsible)),
        Err(Error::RelayInboxWriterNotRecipient)
    ));
    for op in [
        EntryOperation::Overwrite(carrier.clone()),
        EntryOperation::Touch(carrier.clone()),
        EntryOperation::CompactData(carrier.clone()),
    ] {
        assert!(matches!(
            op.validate_inbox_authority(destination, Some(responsible)),
            Err(Error::RelayInboxOperationNotAllowed)
        ));
    }
    assert!(matches!(
        carrier.compact_data(get_epoch_ms(), carrier.clone(), destination),
        Err(Error::RelayInboxOperationNotAllowed)
    ));
    Ok(())
}

#[test]
fn test_relocation_is_accepted_from_the_predecessor_alone() {
    let predecessor = Did::from(3u32);
    assert!(validate_inbox_relocation(predecessor, Some(predecessor)).is_ok());
    assert!(matches!(
        validate_inbox_relocation(Did::from(4u32), Some(predecessor)),
        Err(Error::RelayInboxNotRelocatable)
    ));
    assert!(matches!(
        validate_inbox_relocation(predecessor, None),
        Err(Error::RelayInboxNotRelocatable)
    ));
}

#[test]
fn test_drain_delivers_the_witnessed_elements_and_retires_every_element_by_dot() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.account_did();
    let first = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let second = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let junk = held_by(&holder, destination, TEST_NETWORK_ID + 1)?;

    let mut carrier = carrier_with(Entry::inbox_delta(&first)?, now_ms, actor)?;
    carrier = carrier.extend(now_ms + 1, Entry::inbox_delta(&second)?, actor)?;
    let mut with_junk = Entry::inbox_delta(&junk)?;
    with_junk.data.push(junk.encode()?);
    carrier = carrier.extend(now_ms + 2, with_junk, actor)?;
    assert_eq!(carrier.data.len(), 3);

    let drain = carrier.drain_inbox(now_ms + 3, TEST_NETWORK_ID);
    assert_eq!(
        drain
            .deliverable
            .iter()
            .map(|payload| payload.transaction.tx_id)
            .collect::<Vec<_>>(),
        vec![
            first.payload.transaction.tx_id,
            second.payload.transaction.tx_id
        ]
    );
    assert_eq!(drain.rejected, 1);
    assert_eq!(drain.retired.crdt.dots, carrier.crdt.dots);
    assert!(drain.retired.data.is_empty());

    // Retiring by dot removes exactly the drained elements, and a stale copy that still carries
    // them cannot resurrect them; an element held afterwards is untouched.
    let stale = carrier.clone();
    let retired = carrier.tombstone(drain.retired)?;
    assert!(retired.data.is_empty());
    assert!(retired.join(stale)?.data.is_empty());
    let third = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let after = retired.extend(now_ms + 4, Entry::inbox_delta(&third)?, actor)?;
    assert_eq!(after.data, vec![third.encode()?]);
    Ok(())
}

#[test]
fn test_inbox_keeps_the_newest_elements_and_bounds_its_tombstones() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.account_did();
    let mut carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage);
    let mut newest = None;
    for index in 0..RELAY_INBOX_MAX_LEN + 1 {
        let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
        newest = Some(held.encode()?);
        carrier = carrier.extend(now_ms + index as u128, Entry::inbox_delta(&held)?, actor)?;
    }
    assert_eq!(carrier.data.len(), RELAY_INBOX_MAX_LEN);
    assert_eq!(carrier.data.last(), newest.as_ref());

    let drained = carrier.drain_inbox(now_ms, TEST_NETWORK_ID);
    let retired = carrier.tombstone(drained.retired)?;
    assert!(retired.data.is_empty());
    assert!(retired.crdt.tombstones.len() <= RELAY_INBOX_MAX_LEN);
    Ok(())
}
