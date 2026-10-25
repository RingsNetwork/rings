use super::inbox::inbox_destination;
use super::inbox::inbox_key;
use super::inbox::HeldMessage;
use super::inbox::InboxElement;
use super::Entry;
use super::EntryKind;
use super::EntryOperation;
use super::EntryVersion;
use super::PlacedEntry;
use super::PlacedEntryOperation;
use crate::consts::DEFAULT_RELAY_INBOX_TTL_MS;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::NotifyPredecessorSend;
use crate::tests::with_retention;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

fn session() -> Result<DelegateeKey> {
    DelegateeKey::new_with_seckey(&SecretKey::random())
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
    holder: &DelegateeKey,
    payload: MessagePayload,
    network_id: u32,
    held_at_ms: u128,
) -> Result<HeldMessage> {
    HeldMessage::hold(payload, MessageSigner::new(holder, network_id), held_at_ms)
}

/// A hold by `holder` of a fresh custom message to `destination`, inside `network_id`, now.
fn held_by(holder: &DelegateeKey, destination: Did, network_id: u32) -> Result<HeldMessage> {
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

/// Placement law: a relay carrier has one placement, its DID, whatever redundancy the receiver
/// configures, so no rotated replica key ever admits a hold; a data topic keeps its affine set.
#[test]
fn test_relay_carrier_has_one_placement_whatever_the_redundancy() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let inbox = live_delta(
        &held_by(&holder, destination, TEST_NETWORK_ID)?,
        get_epoch_ms(),
    )?;
    let mut keys = inbox.did.rotate_affine(2)?.into_iter();
    let (Some(own), Some(rotated)) = (keys.next(), keys.next()) else {
        return Err(Error::InvalidAffineScalar);
    };
    assert_eq!(own, inbox.did);

    assert!(PlacedEntry::new(own, inbox.clone())
        .validate_placement(2)
        .is_ok());
    assert!(PlacedEntry::new(rotated, inbox.clone())
        .validate_placement(2)
        .is_err());
    let hold = PlacedEntryOperation {
        placement: rotated,
        op: EntryOperation::Extend(inbox.clone()),
    };
    assert!(hold.validate_placement(2).is_err());

    let topic = Entry::new(inbox.did, Vec::new(), EntryKind::Data);
    assert!(PlacedEntry::new(rotated, topic)
        .validate_placement(2)
        .is_ok());
    Ok(())
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
    assert_eq!(held.holder(), holder.delegator_did());
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
        holder.delegator_did(),
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
            .push(held_by(&holder, destination, TEST_NETWORK_ID)?.to_element()?);
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
    let responsible = holder.delegator_did();
    let stranger: Did = SecretKey::random().address().into();
    let hold = EntryOperation::Extend(live_delta(
        &held_by(&holder, destination, TEST_NETWORK_ID)?,
        now_ms,
    )?);

    hold.validate_inbox_write(responsible, true, now_ms, TEST_NETWORK_ID)?;
    // A writer the holder's view does not admit, and an admitted writer that is not the holder
    // of the elements, are both refused.
    for (writer, admitted) in [(responsible, false), (stranger, true)] {
        assert!(matches!(
            hold.validate_inbox_write(writer, admitted, now_ms, TEST_NETWORK_ID),
            Err(Error::RelayMessageHolderNotResponsible)
        ));
    }

    // A responsible writer relaying another node's hold is not that node's holder.
    let relayed = EntryOperation::Extend(live_delta(
        &held_by(&other, destination, TEST_NETWORK_ID)?,
        now_ms,
    )?);
    assert!(matches!(
        relayed.validate_inbox_write(responsible, true, now_ms, TEST_NETWORK_ID),
        Err(Error::RelayMessageHolderNotResponsible)
    ));

    // A hold whose sender proof has expired by the owner's clock is stale, however the holder
    // stamped it; the witness alone would still admit it.
    let stale_now = now_ms + u128::from(DEFAULT_TTL_MS) + TS_OFFSET_TOLERANCE_MS + 1;
    let stale = EntryOperation::Extend(live_delta(
        &held_by(&holder, destination, TEST_NETWORK_ID)?,
        stale_now,
    )?);
    stale.validate_admissible_at(stale_now, TEST_NETWORK_ID)?;
    assert!(matches!(
        stale.validate_inbox_write(responsible, true, stale_now, TEST_NETWORK_ID),
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
        false,
        now_ms,
        TEST_NETWORK_ID,
    )?;
    assert!(matches!(
        EntryOperation::Tombstone(carrier.clone()).validate_inbox_write(
            responsible,
            true,
            now_ms,
            TEST_NETWORK_ID
        ),
        Err(Error::RelayInboxWriterNotRecipient)
    ));
    assert!(matches!(
        EntryOperation::Overwrite(carrier.clone()).validate_inbox_write(
            destination,
            true,
            now_ms,
            TEST_NETWORK_ID
        ),
        Err(Error::RelayInboxOperationNotAllowed)
    ));
    Ok(())
}

#[test]
fn test_each_deliverable_element_is_paired_with_the_dot_that_retires_it() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.delegator_did();
    let first = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let second = held_by(&holder, destination, TEST_NETWORK_ID)?;

    let carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage)
        .extend(now_ms, Entry::inbox_delta(&first)?, actor)?
        .extend(now_ms + 1, Entry::inbox_delta(&second)?, actor)?;

    let deliverable = carrier.stored_inbox_elements();
    assert!(deliverable
        .iter()
        .all(|element| element.is_deliverable_at(now_ms + 2, TEST_NETWORK_ID)));
    assert_eq!(
        deliverable
            .iter()
            .map(InboxElement::tx_id)
            .collect::<Vec<_>>(),
        vec![
            first.payload.transaction.tx_id,
            second.payload.transaction.tx_id
        ]
    );
    assert_eq!(
        deliverable
            .iter()
            .map(|element| element.dot)
            .collect::<Vec<_>>(),
        carrier.crdt.dots
    );

    // Retiring by dot removes exactly the named element, a stale copy cannot resurrect it, and
    // an element held afterwards is untouched.
    let stale = carrier.clone();
    let mut retired = carrier.clone();
    for element in &deliverable {
        retired = retired.tombstone(carrier.removal_of([element.dot]))?;
    }
    assert!(retired.data.is_empty());
    assert!(retired.join(stale)?.data.is_empty());
    let third = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let after = retired.extend(now_ms + 4, Entry::inbox_delta(&third)?, actor)?;
    assert_eq!(after.data, vec![third.to_element()?]);
    Ok(())
}

#[test]
fn test_inbox_keeps_the_newest_elements_and_bounds_its_tombstones() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let actor = holder.delegator_did();

    // Two full inboxes, drained in turn: the second drain leaves twice the cap of removals to
    // choose from, and the carrier keeps the newest cap of them. Every instant is read from the
    // clock the holder signatures use, so the witness never sees a hold ahead of its judge.
    let mut carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage);
    let mut first_round_dots = Vec::new();
    for round in 0..2 {
        let mut newest = None;
        for _ in 0..RELAY_INBOX_MAX_LEN + 1 {
            let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
            newest = Some(held.to_element()?);
            carrier = carrier.extend(get_epoch_ms(), Entry::inbox_delta(&held)?, actor)?;
        }
        assert_eq!(carrier.data.len(), RELAY_INBOX_MAX_LEN);
        assert_eq!(carrier.data.last(), newest.as_ref());
        if round == 0 {
            first_round_dots = carrier.crdt.dots.clone();
        }
        let removal = carrier.removal_of(
            carrier
                .stored_inbox_elements()
                .iter()
                .map(|element| element.dot),
        );
        carrier = carrier.tombstone(removal)?;
        assert!(carrier.data.is_empty());
    }

    assert_eq!(carrier.crdt.tombstones.len(), RELAY_INBOX_MAX_LEN);
    let kept = carrier
        .crdt
        .tombstones
        .iter()
        .map(|tombstone| tombstone.dot)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(first_round_dots.iter().all(|dot| !kept.contains(dot)));
    Ok(())
}

/// Law (clock gate): after this clock steps back, an element held ahead of it by more than σ
/// is not delivered, and it is delivered once the clock catches up.
#[test]
fn test_an_element_held_ahead_of_a_stepped_back_clock_is_kept_for_a_later_pass() -> Result<()> {
    let now_ms = get_epoch_ms();
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let held = held_by(&holder, destination, TEST_NETWORK_ID)?;
    let carrier = Entry::new(inbox_key(destination), Vec::new(), EntryKind::RelayMessage).extend(
        now_ms,
        Entry::inbox_delta(&held)?,
        holder.delegator_did(),
    )?;

    let stepped_back = now_ms - TS_OFFSET_TOLERANCE_MS - 1;
    let [element] = carrier
        .stored_inbox_elements()
        .try_into()
        .expect("one element");
    assert!(!element.is_deliverable_at(stepped_back, TEST_NETWORK_ID));
    assert!(element.is_deliverable_at(now_ms, TEST_NETWORK_ID));
    Ok(())
}

/// The relay hold horizon (load-bounds paper, Def. 3): a held message retires
/// `MAX_RELAY_INBOX_TTL_MS` after its hold instant. Every read projects it out of its carrier,
/// which stays admissible, the other elements stay, and the recipient never delivers it.
#[test]
fn test_a_hold_past_the_relay_horizon_is_projected_out_and_never_delivered() -> Result<()> {
    let holder = session()?;
    let destination: Did = SecretKey::random().address().into();
    let now_ms = get_epoch_ms();
    let old = hold_at(
        &holder,
        custom_to(destination, TEST_NETWORK_ID)?,
        TEST_NETWORK_ID,
        now_ms,
    )?;
    let horizon_ms = now_ms + u128::from(MAX_RELAY_INBOX_TTL_MS);
    assert!(!old.is_retired_at(horizon_ms - 1));
    assert!(old.is_retired_at(horizon_ms));

    let mut carrier = Entry::inbox_delta(&old)?.try_into_storage_entry()?;
    let young = hold_at(
        &holder,
        custom_to(destination, TEST_NETWORK_ID)?,
        TEST_NETWORK_ID,
        now_ms + 1,
    )?;
    carrier = carrier.join(Entry::inbox_delta(&young)?)?;
    assert_eq!(carrier.data.len(), 2);

    let projected = carrier.clone().retired_at(horizon_ms);
    assert_eq!(projected.data, vec![young.to_element()?]);
    assert_eq!(projected.crdt.dots.len(), 1);
    // The projection is identity before the horizon.
    assert_eq!(carrier.clone().retired_at(horizon_ms - 1), carrier);

    // Read whole, as a carrier fetched before the projection: only the young hold is delivered.
    let deliverable = carrier
        .stored_inbox_elements()
        .into_iter()
        .filter(|element| element.is_deliverable_at(horizon_ms, TEST_NETWORK_ID))
        .map(|element| element.held)
        .collect::<Vec<_>>();
    assert_eq!(deliverable, vec![young]);
    Ok(())
}
