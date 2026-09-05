use super::inbox::inbox_destination;
use super::inbox::inbox_key;
use super::inbox::validate_inbox_relocation;
use super::inbox::HeldMessage;
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
use crate::message::NotifyPredecessorSend;
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

/// A hold by `holder` of `payload` inside `network_id` at `held_at_ms`.
fn hold_at(
    holder: &SessionSk,
    payload: MessagePayload,
    network_id: u32,
    held_at_ms: u128,
) -> Result<HeldMessage> {
    HeldMessage::hold(payload, MessageSigner::new(holder, network_id), held_at_ms)
}

/// A hold by `holder` of a fresh custom message to `destination`, inside `network_id`, now.
fn held_by(holder: &SessionSk, destination: Did, network_id: u32) -> Result<HeldMessage> {
    hold_at(
        holder,
        custom_to(destination, network_id)?,
        network_id,
        get_epoch_ms(),
    )
}

/// The delta of one hold, stamped live at `now_ms` under the inbox policy.
fn live_delta(held: &HeldMessage, now_ms: u128) -> Result<Entry> {
    Ok(with_retention(
        Entry::inbox_delta(held)?,
        now_ms + u128::from(DEFAULT_RELAY_INBOX_TTL_MS),
    ))
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
    let delta = live_delta(&held, now_ms)?;

    assert_eq!(delta.did, inbox_key(destination));
    delta.validate_admissible_at(now_ms, TEST_NETWORK_ID)?;
    assert_eq!(held.holder(), holder.account_did());
    Ok(())
}

#[test]
fn test_witness_rejects_a_message_addressed_elsewhere() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let mut misfiled = live_delta(&held, now_ms)?;
    misfiled.did = inbox_key(SecretKey::random().address().into());
    assert!(matches!(
        misfiled.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageNotAddressedToInbox)
    ));

    // The relay destination is bound too: the recipient must have nothing to forward.
    let mut forwarding = custom_to(destination, TEST_NETWORK_ID)?;
    forwarding.relay.destination = SecretKey::random().address().into();
    let held = hold_at(&holder, forwarding, TEST_NETWORK_ID, now_ms)?;
    assert!(matches!(
        held.validate_witness(destination, now_ms, TEST_NETWORK_ID),
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
    let held = hold_at(&holder, control, TEST_NETWORK_ID, now_ms)?;

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

    let foreign_hold = hold_at(
        &holder,
        custom_to(destination, TEST_NETWORK_ID)?,
        TEST_NETWORK_ID + 1,
        now_ms,
    )?;
    assert!(matches!(
        foreign_hold.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageUnverifiable)
    ));

    let foreign_payload = hold_at(
        &holder,
        custom_to(destination, TEST_NETWORK_ID + 1)?,
        TEST_NETWORK_ID,
        now_ms,
    )?;
    assert!(matches!(
        foreign_payload.validate_witness(destination, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageHeldOutsideSenderProof)
    ));
    Ok(())
}

#[test]
fn test_witness_bounds_the_hold_instant_by_the_receiver_clock() -> Result<()> {
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&session()?, destination, TEST_NETWORK_ID)?;
    let held_at = held.held_at_ms();

    // The boundary is admissible; one millisecond beyond the tolerance is not.
    held.validate_witness(
        destination,
        held_at - TS_OFFSET_TOLERANCE_MS,
        TEST_NETWORK_ID,
    )?;
    assert!(matches!(
        held.validate_witness(
            destination,
            held_at - TS_OFFSET_TOLERANCE_MS - 1,
            TEST_NETWORK_ID
        ),
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
    let held_late = hold_at(&holder, payload.clone(), TEST_NETWORK_ID, late)?;
    assert!(matches!(
        held_late.validate_witness(destination, late, TEST_NETWORK_ID),
        Err(Error::RelayMessageHeldOutsideSenderProof)
    ));

    // A hold inside it stays admissible however far the receiver's clock has moved on: the
    // witness is monotone in `now_ms`.
    let held = hold_at(&holder, payload, TEST_NETWORK_ID, get_epoch_ms())?;
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
    let mut with_floor = live_delta(&held, now_ms)?;
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

    with_retention(delta.clone(), now_ms + u128::from(MAX_RELAY_INBOX_TTL_MS))
        .validate_admissible_at(now_ms, TEST_NETWORK_ID)?;
    let too_long = with_retention(
        delta,
        now_ms + u128::from(MAX_RELAY_INBOX_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1,
    );
    assert!(matches!(
        too_long.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::EntryLifetimeExceedsMax)
    ));
    assert!(u128::from(MAX_TTL_MS) < u128::from(MAX_RELAY_INBOX_TTL_MS));
    Ok(())
}

#[test]
fn test_witness_refuses_a_delta_larger_than_the_inbox_before_verifying() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let mut delta = live_delta(&held_by(&holder, destination, TEST_NETWORK_ID)?, now_ms)?;
    for _ in 0..RELAY_INBOX_MAX_LEN {
        delta
            .data
            .push(held_by(&holder, destination, TEST_NETWORK_ID)?.encode()?);
    }
    assert_eq!(delta.data.len(), RELAY_INBOX_MAX_LEN + 1);
    assert!(matches!(
        delta.validate_admissible_at(now_ms, TEST_NETWORK_ID),
        Err(Error::RelayInboxDeltaExceedsCapacity)
    ));
    Ok(())
}

#[test]
fn test_hold_law_admits_only_the_responsible_holder_of_a_live_message() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let other = session()?;
    let destination: Did = SecretKey::random().address().into();
    let responsible = holder.account_did();
    let stranger: Did = SecretKey::random().address().into();
    let hold = EntryOperation::Extend(live_delta(
        &held_by(&holder, destination, TEST_NETWORK_ID)?,
        now_ms,
    )?);

    hold.validate_inbox_write(responsible, Some(responsible), now_ms, TEST_NETWORK_ID)?;
    for (writer, node) in [
        (stranger, Some(responsible)),
        (responsible, Some(stranger)),
        (responsible, None),
    ] {
        assert!(matches!(
            hold.validate_inbox_write(writer, node, now_ms, TEST_NETWORK_ID),
            Err(Error::RelayMessageHolderNotResponsible)
        ));
    }

    // A responsible writer relaying another node's hold is not that node's holder.
    let relayed = EntryOperation::Extend(live_delta(
        &held_by(&other, destination, TEST_NETWORK_ID)?,
        now_ms,
    )?);
    assert!(matches!(
        relayed.validate_inbox_write(responsible, Some(responsible), now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageHolderNotResponsible)
    ));

    // A hold whose sender proof has expired by the owner's clock is stale, however the holder
    // stamped it; the witness alone would still admit it.
    let stale_now = now_ms + u128::from(DEFAULT_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1;
    let stale = EntryOperation::Extend(live_delta(
        &held_by(&holder, destination, TEST_NETWORK_ID)?,
        stale_now,
    )?);
    stale
        .entry()
        .validate_admissible_at(stale_now, TEST_NETWORK_ID)?;
    assert!(matches!(
        stale.validate_inbox_write(responsible, Some(responsible), stale_now, TEST_NETWORK_ID),
        Err(Error::RelayMessageHoldStale)
    ));
    Ok(())
}

#[test]
fn test_removal_authority_is_the_recipient_alone_and_nothing_else_is_allowed() -> Result<()> {
    let now_ms = get_epoch_ms();
    let destination: Did = SecretKey::random().address().into();
    let carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage);
    let responsible: Did = SecretKey::random().address().into();

    EntryOperation::Tombstone(carrier.clone()).validate_inbox_write(
        destination,
        Some(responsible),
        now_ms,
        TEST_NETWORK_ID,
    )?;
    assert!(matches!(
        EntryOperation::Tombstone(carrier.clone()).validate_inbox_write(
            responsible,
            Some(responsible),
            now_ms,
            TEST_NETWORK_ID
        ),
        Err(Error::RelayInboxWriterNotRecipient)
    ));
    for op in [
        EntryOperation::Overwrite(carrier.clone()),
        EntryOperation::Touch(carrier.clone()),
        EntryOperation::CompactData(carrier.clone()),
    ] {
        assert!(matches!(
            op.validate_inbox_write(destination, Some(responsible), now_ms, TEST_NETWORK_ID),
            Err(Error::RelayInboxOperationNotAllowed)
        ));
    }
    assert!(matches!(
        carrier.compact_data(now_ms, carrier.clone(), destination),
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
fn test_partition_pairs_each_witnessed_element_with_its_dot_and_retires_the_rest() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.account_did();
    let first = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let second = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let junk = held_by(&holder, destination, TEST_NETWORK_ID + 1)?;

    let carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage)
        .extend(now_ms, Entry::inbox_delta(&first)?, actor)?
        .extend(now_ms + 1, Entry::inbox_delta(&second)?, actor)?
        .extend(now_ms + 2, Entry::inbox_delta(&junk)?, actor)?;
    assert_eq!(carrier.data.len(), 3);

    let drain = carrier.partition_inbox(now_ms + 3, TEST_NETWORK_ID);
    assert_eq!(
        drain
            .deliverable
            .iter()
            .map(|element| element.payload.transaction.tx_id)
            .collect::<Vec<_>>(),
        vec![
            first.payload.transaction.tx_id,
            second.payload.transaction.tx_id
        ]
    );
    let delivered_dots = drain
        .deliverable
        .iter()
        .map(|element| element.dot)
        .chain(drain.rejected.crdt.dots.iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(delivered_dots, carrier.crdt.dots.iter().copied().collect());
    assert_eq!(drain.rejected.crdt.dots.len(), 1);

    // Retiring by dot removes exactly the named element, a stale copy cannot resurrect it, and
    // an element held afterwards is untouched.
    let stale = carrier.clone();
    let mut retired = carrier.tombstone(drain.rejected)?;
    for element in &drain.deliverable {
        retired = retired.tombstone(carrier.removal_of([element.dot]))?;
    }
    assert!(retired.data.is_empty());
    assert!(retired.join(stale)?.data.is_empty());
    let third = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let after = retired.extend(now_ms + 4, Entry::inbox_delta(&third)?, actor)?;
    assert_eq!(after.data, vec![third.encode()?]);
    Ok(())
}

#[test]
fn test_inbox_keeps_the_newest_elements_and_bounds_its_tombstones() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.account_did();

    // Two full inboxes, drained in turn: the second drain leaves twice the cap of removals to
    // choose from, and the carrier keeps the newest cap of them. Every instant is read from the
    // clock the holder signatures use, so the witness never sees a hold ahead of its judge.
    let mut carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage);
    let mut first_round_dots = Vec::new();
    for round in 0..2 {
        let mut newest = None;
        for _ in 0..RELAY_INBOX_MAX_LEN + 1 {
            let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
            newest = Some(held.encode()?);
            carrier = carrier.extend(get_epoch_ms(), Entry::inbox_delta(&held)?, actor)?;
        }
        assert_eq!(carrier.data.len(), RELAY_INBOX_MAX_LEN);
        assert_eq!(carrier.data.last(), newest.as_ref());
        if round == 0 {
            first_round_dots = carrier.crdt.dots.clone();
        }
        let drained = carrier.partition_inbox(get_epoch_ms(), TEST_NETWORK_ID);
        let removal = carrier.removal_of(drained.deliverable.iter().map(|element| element.dot));
        carrier = carrier.tombstone(removal)?;
        assert!(carrier.data.is_empty());
    }

    assert_eq!(carrier.crdt.tombstones.len(), RELAY_INBOX_MAX_LEN);
    let kept = carrier
        .crdt
        .tombstones
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(first_round_dots.iter().all(|dot| !kept.contains(dot)));
    Ok(())
}
