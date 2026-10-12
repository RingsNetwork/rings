//! E2E handshake, stream encryption and streaming-decryption tests.

use rand::SeedableRng;
use rand_hc::Hc128Rng;

use super::*;

fn recipient_key() -> SecretKey {
    SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0").unwrap()
}

fn sender_key() -> SecretKey {
    SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54").unwrap()
}

#[test]
fn test_public_key_must_match_did() {
    let sender = sender_key();
    let recipient = recipient_key();

    assert!(ensure_public_key_matches_did(sender.pubkey(), sender.address().into()).is_ok());
    assert!(matches!(
        ensure_public_key_matches_did(sender.pubkey(), recipient.address().into()),
        Err(Error::E2ePublicKeyDidMismatch { .. })
    ));
}

#[test]
fn test_handshake_messages_verify_signed_owner() {
    let sender = sender_key();
    let recipient = recipient_key();
    let request = E2eHandshakeRequest::new(sender.pubkey());
    let response = E2eHandshakeResponse::new(recipient.pubkey());

    request.verify_requester(sender.address().into()).unwrap();
    response
        .verify_responder(recipient.address().into())
        .unwrap();
    assert!(request
        .verify_requester(recipient.address().into())
        .is_err());
    assert!(response.verify_responder(sender.address().into()).is_err());
}

#[test]
fn test_round_trip_random_binary_payloads_and_frame_sizes() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(608);
    let payload_lens = [0usize, 1, 15, 16, 17, 31, 32, 255, 1024, 4097];
    let frame_limits = [1usize, E2E_PLAINTEXT_BLOCK_LEN, 64, 511];

    for payload_len in payload_lens {
        for frame_limit in frame_limits {
            let mut payload = vec![0u8; payload_len];
            rng.fill_bytes(&mut payload);
            let stream_id = uuid::Uuid::new_v4();

            let frames = encrypt_stream_with_rng(
                &payload,
                stream_id,
                sender.pubkey(),
                recipient.pubkey(),
                frame_limit,
                &mut rng,
            )
            .unwrap();
            assert_eq!(
                frames.len(),
                payload_len.div_ceil(frame_limit.max(1)).max(1)
            );
            assert_eq!(
                decrypt_stream(
                    &frames,
                    stream_id,
                    sender.address().into(),
                    recipient.clone(),
                )
                .unwrap(),
                payload
            );
        }
    }
}

#[test]
fn test_empty_plaintext_sends_final_stream_frame() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(7);
    let stream_id = uuid::Uuid::new_v4();

    let frames = encrypt_stream_with_rng(
        &[],
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        8,
        &mut rng,
    )
    .unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].stream_id, stream_id);
    assert_eq!(frames[0].sequence, 0);
    assert!(frames[0].is_final);
    assert_eq!(
        decrypt_stream(&frames, stream_id, sender.address().into(), recipient).unwrap(),
        Vec::<u8>::new()
    );
}

#[test]
fn test_streaming_decryptor_accepts_ordered_final_frame() {
    let sender = sender_key();
    let recipient = recipient_key();
    let stream_id = uuid::Uuid::new_v4();
    let mut rng = Hc128Rng::seed_from_u64(42);
    let mut encryptor =
        E2eStreamEncryptor::new(stream_id, sender.pubkey(), recipient.pubkey()).unwrap();

    let first = encryptor.encrypt_next(b"\0hello", &mut rng).unwrap();
    let second = encryptor.encrypt_final(b"\xFFworld", &mut rng).unwrap();
    let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0);

    let mut plaintext = decryptor.decrypt_next(&first, 0).unwrap();
    plaintext.extend_from_slice(&decryptor.decrypt_next(&second, 0).unwrap());
    decryptor.finish().unwrap();

    assert_eq!(plaintext, b"\0hello\xFFworld");
}

#[test]
fn test_truncation_is_rejected_without_final_frame() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(9);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![9u8; 96];
    let mut frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();

    frames.pop();

    assert!(matches!(
        decrypt_stream(&frames, stream_id, sender.address().into(), recipient),
        Err(Error::E2eMissingFinalFrame)
    ));
}

#[test]
fn test_reordered_frames_are_buffered_until_contiguous() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(10);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![10u8; 96];
    let mut frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();

    frames.swap(0, 1);

    assert_eq!(
        decrypt_stream(&frames, stream_id, sender.address().into(), recipient).unwrap(),
        payload
    );
}

#[test]
fn test_replayed_consumed_frame_is_idempotent() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(16);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![16u8; 96];
    let frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();
    let final_frame = frames.last().unwrap().clone();
    let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0);
    let mut plaintext = Vec::new();

    plaintext.extend_from_slice(&decryptor.decrypt_next(&frames[0], 0).unwrap());
    assert_eq!(
        decryptor.decrypt_next(&frames[0], 0).unwrap(),
        Vec::<u8>::new()
    );

    for frame in &frames[1..] {
        plaintext.extend_from_slice(&decryptor.decrypt_next(frame, 0).unwrap());
    }
    decryptor.finish().unwrap();

    assert_eq!(
        decryptor.decrypt_next(&final_frame, 0).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(plaintext, payload);
}

#[test]
fn test_replayed_buffered_frame_is_idempotent() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(17);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![17u8; 96];
    let frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();
    let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0);
    let mut plaintext = Vec::new();

    assert_eq!(
        decryptor.decrypt_next(&frames[1], 0).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        decryptor.decrypt_next(&frames[1], 0).unwrap(),
        Vec::<u8>::new()
    );

    for frame in &frames {
        plaintext.extend_from_slice(&decryptor.decrypt_next(frame, 0).unwrap());
    }
    decryptor.finish().unwrap();

    assert_eq!(plaintext, payload);
}

#[test]
fn test_future_frame_outside_reorder_window_is_rejected() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(18);
    let stream_id = uuid::Uuid::new_v4();
    let frames = encrypt_stream_with_rng(
        &[18u8; 5],
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        1,
        &mut rng,
    )
    .unwrap();
    let mut decryptor = E2eStreamDecryptor::with_reorder_window(
        stream_id,
        sender.address().into(),
        recipient,
        2,
        0,
    );

    assert!(matches!(
        decryptor.decrypt_next(&frames[3], 0),
        Err(Error::E2eFrameReorderWindowExceeded {
            next_sequence: 0,
            actual: 3,
            window: 2
        })
    ));
}

#[test]
fn test_final_frame_can_arrive_before_gap_is_filled() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(14);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![14u8; 96];
    let mut frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();
    frames.rotate_right(1);

    let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0);
    let mut plaintext = Vec::new();

    assert_eq!(
        decryptor.decrypt_next(&frames[0], 0).unwrap(),
        Vec::<u8>::new()
    );
    assert!(matches!(
        decryptor.finish(),
        Err(Error::E2eMissingFinalFrame)
    ));

    for frame in &frames[1..] {
        plaintext.extend_from_slice(&decryptor.decrypt_next(frame, 0).unwrap());
    }
    decryptor.finish().unwrap();

    assert_eq!(plaintext, payload);
}

#[test]
fn test_frame_after_buffered_final_is_rejected() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(15);
    let stream_id = uuid::Uuid::new_v4();
    let frames = encrypt_stream_with_rng(
        &[15u8; 96],
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();
    let final_frame = frames.last().unwrap();
    let mut frame_after_final = frames.first().unwrap().clone();
    frame_after_final.sequence = final_frame.sequence.checked_add(1).unwrap();

    let mut decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0);
    assert_eq!(
        decryptor.decrypt_next(final_frame, 0).unwrap(),
        Vec::<u8>::new()
    );
    assert!(matches!(
        decryptor.decrypt_next(&frame_after_final, 0),
        Err(Error::E2eFrameAfterFinal)
    ));
}

#[test]
fn test_wrong_stream_id_is_rejected() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(12);
    let stream_id = uuid::Uuid::new_v4();
    let other_stream_id = uuid::Uuid::new_v4();
    let frames = encrypt_stream_with_rng(
        b"hello",
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();

    assert!(matches!(
        decrypt_stream(&frames, other_stream_id, sender.address().into(), recipient),
        Err(Error::E2eStreamIdMismatch { .. })
    ));
}

#[test]
fn test_wrong_sender_key_is_rejected() {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(11);
    let stream_id = uuid::Uuid::new_v4();
    let payload = vec![11u8; 32];
    let mut frames = encrypt_stream_with_rng(
        &payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        32,
        &mut rng,
    )
    .unwrap();
    frames[0].sender_public_key = recipient.pubkey();

    assert!(matches!(
        decrypt_stream(&frames, stream_id, sender.address().into(), recipient),
        Err(Error::E2ePublicKeyDidMismatch { .. })
    ));
}

/// The stall deadline of the stall tests, in milliseconds.
const STALL_MS: u128 = 1_000;

/// A stream of five frames of `payload`, and a decryptor for it created at time 0 with the
/// stall deadline [`STALL_MS`].
fn stalling_stream(payload: &[u8]) -> (Vec<E2eStreamFrame>, E2eStreamDecryptor) {
    let sender = sender_key();
    let recipient = recipient_key();
    let mut rng = Hc128Rng::seed_from_u64(23);
    let stream_id = uuid::Uuid::new_v4();
    let frames = encrypt_stream_with_rng(
        payload,
        stream_id,
        sender.pubkey(),
        recipient.pubkey(),
        16,
        &mut rng,
    )
    .unwrap();
    assert_eq!(frames.len(), 5);
    let decryptor = E2eStreamDecryptor::new(stream_id, sender.address().into(), recipient, 0)
        .with_stall_deadline(std::time::Duration::from_millis(STALL_MS as u64));
    (frames, decryptor)
}

/// A lost first, middle or final frame stalls the stream: the plaintext released is the
/// prefix before the loss, and the stream fails once it has made no progress for its stall
/// deadline, whether later frames wait buffered or nothing more arrives.
#[test]
fn test_a_lost_frame_stalls_the_stream_after_its_prefix() {
    let payload: Vec<u8> = (0..80).collect();
    for lost in 0..5u64 {
        let (frames, mut decryptor) = stalling_stream(&payload);
        let mut plaintext = Vec::new();
        for frame in frames.iter().filter(|frame| frame.sequence != lost) {
            plaintext.extend(decryptor.decrypt_next(frame, 10).unwrap());
        }
        let prefix_len = usize::try_from(lost).unwrap() * 16;
        assert_eq!(plaintext, payload[..prefix_len], "frame {lost} lost");
        // Progress is the creation (no frame released) or the frames at time 10.
        assert!(decryptor.check(STALL_MS).is_ok(), "within its deadline");
        assert!(
            matches!(
                decryptor.check(11 + STALL_MS),
                Err(Error::E2eStreamStalled { next_sequence, .. }) if next_sequence == lost
            ),
            "frame {lost} lost: the stream stalls at it"
        );
    }
}

/// Progress restarts the deadline, a frame that fills a gap in time is no stall, and a
/// complete stream never stalls.
#[test]
fn test_progress_restarts_the_stall_deadline_and_completion_ends_it() {
    let payload: Vec<u8> = (0..80).collect();
    let (frames, mut decryptor) = stalling_stream(&payload);
    let mut plaintext = decryptor.decrypt_next(&frames[0], STALL_MS).unwrap();
    // Frame 2 overtakes frame 1, which fills the gap just before the deadline.
    assert!(decryptor
        .decrypt_next(&frames[2], STALL_MS)
        .unwrap()
        .is_empty());
    plaintext.extend(decryptor.decrypt_next(&frames[1], 2 * STALL_MS).unwrap());
    // A frame that fills no gap past the deadline finds the stream stalled.
    let (late, mut stalled) = stalling_stream(&payload);
    assert!(stalled.decrypt_next(&late[1], 0).unwrap().is_empty());
    assert!(matches!(
        stalled.decrypt_next(&late[2], STALL_MS + 1),
        Err(Error::E2eStreamStalled {
            next_sequence: 0,
            buffered: 2,
            ..
        })
    ));
    for frame in &frames[3..] {
        plaintext.extend(decryptor.decrypt_next(frame, 2 * STALL_MS).unwrap());
    }
    decryptor.finish().unwrap();
    assert_eq!(plaintext, payload);
    assert!(
        decryptor.check(u128::MAX).is_ok(),
        "a complete stream never stalls"
    );
}
