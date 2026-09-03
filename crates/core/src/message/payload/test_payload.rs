use rand::Rng;

use super::*;
use crate::ecc::SecretKey;
use crate::message::Message;
use crate::session::SessionSk;
use crate::tests::TEST_NETWORK_ID;

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct TestData {
    a: String,
    b: i64,
    c: f64,
    d: bool,
}

pub fn new_test_payload(next_hop: Did) -> MessagePayload {
    let test_data = TestData {
        a: "hello".to_string(),
        b: 111,
        c: 2.33,
        d: true,
    };
    new_payload(test_data, next_hop)
}

pub fn new_payload<T>(data: T, next_hop: Did) -> MessagePayload
where T: Serialize + DeserializeOwned {
    let key = SecretKey::random();
    let destination = SecretKey::random().address().into();
    let session_sk = SessionSk::new_with_seckey(&key).unwrap();
    MessagePayload::new_send(
        data,
        MessageSigner::new(&session_sk, TEST_NETWORK_ID),
        next_hop,
        destination,
    )
    .unwrap()
}

#[test]
fn test_new_then_verify() {
    let key2 = SecretKey::random();
    let did2 = key2.address().into();

    let payload = new_test_payload(did2);
    assert!(payload.verify(TEST_NETWORK_ID));
}

#[test]
fn test_transaction_debug_reports_size_without_data_bytes() -> Result<()> {
    let next_hop = SecretKey::random().address().into();
    let payload = new_payload(Message::custom(&[171; 32])?, next_hop);
    let debug = format!("{:?}", payload.transaction);

    assert!(debug.contains("data_bytes"));
    assert!(!debug.contains("171, 171"));
    Ok(())
}

#[test]
fn test_relay_destination_predicates_name_forwarding_state() -> Result<()> {
    let local_key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&local_key)?;
    let local: Did = local_key.address().into();
    let remote: Did = SecretKey::random().address().into();

    let local_payload = MessagePayload::new_send(
        Message::custom(b"local")?,
        MessageSigner::new(&session_sk, TEST_NETWORK_ID),
        local,
        local,
    )?;
    assert!(local_payload.is_relay_destination_for(local));
    assert!(!local_payload.should_forward_from(local));

    let remote_payload = MessagePayload::new_send(
        Message::custom(b"remote")?,
        MessageSigner::new(&session_sk, TEST_NETWORK_ID),
        remote,
        remote,
    )?;
    assert!(!remote_payload.is_relay_destination_for(local));
    assert!(remote_payload.should_forward_from(local));

    Ok(())
}

#[test]
fn test_report_return_policy_is_signed_by_transaction() -> Result<()> {
    let sender_key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&sender_key)?;
    let sender = session_sk.account_did();
    let destination: Did = SecretKey::random().address().into();

    let mut transaction = Transaction::new_with_report_return(
        destination,
        uuid::Uuid::new_v4(),
        Message::custom(b"policy")?,
        ReportReturnPolicy::Routed {
            destination: sender,
        },
        MessageSigner::new(&session_sk, TEST_NETWORK_ID),
    )?;
    assert!(transaction.verify(TEST_NETWORK_ID));

    transaction.report_return = ReportReturnPolicy::Path;
    assert!(!transaction.verify(TEST_NETWORK_ID));
    Ok(())
}

#[test]
fn test_routed_report_return_destination_must_match_signer() -> Result<()> {
    let sender_key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&sender_key)?;
    let destination: Did = SecretKey::random().address().into();
    let unrelated_return: Did = SecretKey::random().address().into();

    assert!(matches!(
        Transaction::new_with_report_return(
            destination,
            uuid::Uuid::new_v4(),
            Message::custom(b"policy")?,
            ReportReturnPolicy::Routed {
                destination: unrelated_return,
            },
            MessageSigner::new(&session_sk, TEST_NETWORK_ID),
        ),
        Err(Error::InvalidMessage(_))
    ));
    Ok(())
}

/// The sender cuts chunk data at `max_message_size - (MAX_CHUNK_ENVELOPE_OVERHEAD +
/// TRANSPORT_CUSTOM_OVERHEAD)`. This pins that those reserves are large enough by measuring the
/// *exact* bytes the data channel carries: a full-size chunk, re-wrapped in its `MessagePayload`
/// **and** the outer `TransportMessage::Custom` frame (what `send_data` actually serializes),
/// stays at or below `MAX_DATA_CHANNEL_MESSAGE_SIZE`. If either envelope grows past its reserve,
/// this fails instead of silently producing oversized frames the channel would reject.
#[test]
fn test_chunk_envelope_fits_reserve() {
    use rings_transport::core::transport::TransportMessage;
    use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    use crate::chunk::ChunkList;
    use crate::consts::MAX_CHUNK_ENVELOPE_OVERHEAD;
    use crate::consts::TRANSPORT_CUSTOM_OVERHEAD;

    let next_hop = SecretKey::random().address().into();
    let chunk_size =
        MAX_DATA_CHANNEL_MESSAGE_SIZE - (MAX_CHUNK_ENVELOPE_OVERHEAD + TRANSPORT_CUSTOM_OVERHEAD);
    let data: Bytes = vec![0xab; chunk_size].into();
    let chunk = ChunkList::split(&data, chunk_size)
        .to_vec()
        .pop()
        .expect("one chunk");

    // The bytes actually handed to SCTP: rings codec(Custom(rings codec(MessagePayload))).
    let payload_bytes = new_payload(Message::Chunk(chunk), next_hop)
        .to_wire()
        .unwrap();
    let wire = rings_codec::serialize(&TransportMessage::Custom(payload_bytes)).unwrap();

    assert!(
        wire.len() <= MAX_DATA_CHANNEL_MESSAGE_SIZE,
        "wrapped chunk frame is {} bytes, exceeds limit {}; raise the reserves",
        wire.len(),
        MAX_DATA_CHANNEL_MESSAGE_SIZE,
    );
}

/// The other framing boundary: a payload [`WireReserves::plan`] keeps `Whole`, once wrapped in
/// the outer `TransportMessage::Custom` frame, stays within the limit — pinning that
/// `WireReserves::PRODUCTION.whole` is enough for the whole-message path (not just the chunk
/// path), and that one byte past the boundary switches to chunked.
#[test]
fn test_whole_message_boundary_fits_custom_wrapper() {
    use rings_transport::core::transport::TransportMessage;
    use rings_transport::core::transport::MAX_DATA_CHANNEL_MESSAGE_SIZE;

    use crate::chunk::Framing;
    use crate::chunk::WireReserves;

    let reserves = WireReserves::PRODUCTION;
    let limit = MAX_DATA_CHANNEL_MESSAGE_SIZE;
    // Largest payload that should still be sent whole.
    let payload_len = limit - reserves.whole;
    assert_eq!(reserves.plan(payload_len, limit), Some(Framing::Whole));

    let wire = rings_codec::serialize(&TransportMessage::Custom(Bytes::from(vec![
        0u8;
        payload_len
    ])))
    .unwrap();
    assert!(
        wire.len() <= limit,
        "whole wire {} exceeds limit {}",
        wire.len(),
        limit
    );
    // One byte past the boundary must switch to chunked.
    assert!(matches!(
        reserves.plan(payload_len + 1, limit),
        Some(Framing::Chunked { .. })
    ));
}

#[test]
fn test_message_payload_from_auto() {
    let next_hop = SecretKey::random().address().into();

    let payload = new_test_payload(next_hop);
    let gzipped_encoded_payload = payload.encode().unwrap();
    let payload2: MessagePayload = gzipped_encoded_payload.decode().unwrap();
    assert_eq!(payload, payload2);

    let gunzip_encoded_payload = payload.to_wire().unwrap().encode().unwrap();
    let payload2: MessagePayload = gunzip_encoded_payload.decode().unwrap();
    assert_eq!(payload, payload2);
}

#[test]
fn test_message_payload_encode_len() {
    let next_hop = SecretKey::random().address().into();
    let data = rand::thread_rng().gen::<[u8; 32]>();

    let data1 = data;
    let msg1 = Message::custom(&data1).unwrap();
    let payload1 = new_payload(msg1, next_hop);
    let bytes1 = payload1.to_wire().unwrap();
    let encoded1 = payload1.encode().unwrap();
    let encoded_bytes1: Vec<u8> = encoded1.into();

    let data2 = data.repeat(2);
    let msg2 = Message::custom(&data2).unwrap();
    let payload2 = new_payload(msg2, next_hop);
    let bytes2 = payload2.to_wire().unwrap();
    let encoded2 = payload2.encode().unwrap();
    let encoded_bytes2: Vec<u8> = encoded2.into();

    assert_eq!(bytes1.len() - data1.len(), bytes2.len() - data2.len());
    assert_ne!(
        encoded_bytes1.len() - data1.len(),
        encoded_bytes2.len() - data2.len()
    );
}

#[test]
fn test_wire_size_counts_large_verification_fields_without_allocating_wire() -> Result<()> {
    let next_hop = SecretKey::random().address().into();
    let mut payload = new_payload(Message::custom(b"body")?, next_hop);
    payload.verification.sig = vec![9; 64 * 1024];
    payload.transaction.verification.sig = vec![7; 32 * 1024];

    assert_eq!(payload.wire_size()?, payload.to_wire()?.len());
    Ok(())
}

#[test]
fn test_wire_size_matches_every_message_discriminant_and_signature_width() -> Result<()> {
    let next_hop = SecretKey::random().address().into();
    for signature_len in [
        0, 1, 63, 64, 127, 128, 255, 256, 16_383, 16_384, 65_535, 65_536,
    ] {
        for message in Message::test_variants() {
            let message_kind = message.kind().as_str();
            let mut payload = new_payload(message, next_hop);
            payload.verification.sig = vec![9; signature_len];
            payload.transaction.verification.sig = vec![7; signature_len / 2];

            assert_eq!(
                payload.wire_size()?,
                payload.to_wire()?.len(),
                "message kind {message_kind}, signature length {signature_len}"
            );
        }
    }
    Ok(())
}

/// Domain separation: the transaction and payload signatures cover the same hash under distinct
/// tags, so one can never stand in for the other.
#[test]
fn test_transaction_and_payload_signatures_are_not_interchangeable() {
    let next_hop = SecretKey::random().address().into();
    let mut payload = new_test_payload(next_hop);
    assert!(payload.verify(TEST_NETWORK_ID));
    assert!(payload.transaction.verify(TEST_NETWORK_ID));

    payload.verification = payload.transaction.verification.clone();
    assert!(!payload.verify(TEST_NETWORK_ID));
}

/// Overlay binding: both signatures verify only under the overlay they were issued for.
#[test]
fn test_payload_signed_for_another_overlay_is_rejected() {
    let next_hop = SecretKey::random().address().into();
    let payload = new_test_payload(next_hop);

    assert!(!payload.verify(TEST_NETWORK_ID + 1));
    assert!(!payload.transaction.verify(TEST_NETWORK_ID + 1));
}
