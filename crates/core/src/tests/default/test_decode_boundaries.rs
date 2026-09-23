use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use uuid::Uuid;

use crate::chunk::Chunk;
use crate::chunk::ChunkMeta;
use crate::chunk::MessageReassembler;
use crate::chunk::ReassemblyLimits;
use crate::consts::DEFAULT_TTL_MS;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::message::DelegationRef;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::LinkControl;
use crate::message::LinkFrame;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::MessageVerificationExt;
use crate::message::PerSlot;
use crate::message::WirePayload;
use crate::swarm::session_link::FrameArrival;
use crate::swarm::session_link::ReferencedDelegations;
use crate::swarm::session_link::REFERENCED_TABLE_CAPACITY;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

const SEED_ENV: &str = "RINGS_DECODE_BOUNDARY_SEED";
const CASES_ENV: &str = "RINGS_DECODE_BOUNDARY_CASES";
const DEFAULT_CASES: usize = 128;
/// Frames the generated link receiver may hold: small, so the bound is reached often.
const LINK_HOLD_CAPACITY: usize = 4;
/// How long the generated link receiver holds a frame.
const LINK_HOLD_TIMEOUT_MS: u128 = 1_000;

#[test]
fn generated_message_decode_boundary_inputs_are_total() {
    let mut generator = DecodeBoundaryGenerator::named("core-message");
    let cases = generated_case_count();
    eprintln!(
        "{SEED_ENV}={} {CASES_ENV}={cases} target=core-message",
        generator.seed()
    );

    for _ in 0..cases {
        let raw = generator.bytes(4096);
        exercise_message_decode_boundary(&raw);
        if let Some(payload) = generated_payload(&mut generator) {
            exercise_payload_wire_forms(&payload);
        }
    }
}

#[test]
fn generated_chunk_decode_boundary_sequences_respect_bounds() {
    let mut generator = DecodeBoundaryGenerator::named("core-chunk");
    let cases = generated_case_count();
    let limits = generated_reassembly_limits();
    let mut reassembler = MessageReassembler::with_limits(limits);
    eprintln!(
        "{SEED_ENV}={} {CASES_ENV}={cases} target=core-chunk",
        generator.seed()
    );

    for _ in 0..cases {
        if generator.one_in(5) {
            reassembler.remove_expired();
        }
        let chunk = if generator.one_in(3) {
            Chunk::from_wire(&generator.bytes(512)).ok()
        } else {
            Some(generator.chunk(get_epoch_ms()))
        };
        if let Some(chunk) = chunk {
            let output = reassembler.handle(chunk);
            assert!(reassembler.pending_count() <= limits.max_pending_messages.max(1));
            if let Some(bytes) = output {
                assert!(bytes.len() <= limits.max_message_bytes.max(1));
            }
        }
    }
}

/// Law (bound, under arbitrary input): whatever frames a peer sends, the receiving end of its
/// link holds at most its hold capacity in frames and its table capacity in sessions, and no
/// step fails other than by a typed error. Inputs are valid link frames (inline, referenced,
/// and control) with generated mutations, so the decoder is exercised past its marker.
#[test]
fn generated_link_frame_decode_boundary_inputs_keep_the_receiver_bounded() {
    let mut generator = DecodeBoundaryGenerator::named("core-link-frame");
    let cases = generated_case_count();
    let mut receiver = ReferencedDelegations::new(LINK_HOLD_CAPACITY, LINK_HOLD_TIMEOUT_MS);
    // Sessions earlier frames referenced: an announcement of one of them is awaited, so the
    // announce, release and sweep paths run under generated input, not only the refusals.
    let mut referenced: Vec<Delegation> = Vec::new();
    eprintln!(
        "{SEED_ENV}={} {CASES_ENV}={cases} target=core-link-frame",
        generator.seed()
    );

    for case in 0..cases {
        let Some(wire) = generated_link_frame(&mut generator, &mut referenced) else {
            continue;
        };
        let wire = generator.mutated(wire);
        let now_ms = get_epoch_ms();
        match LinkFrame::from_wire(&wire) {
            Ok(LinkFrame::Payload(frame)) => {
                if let Ok(FrameArrival::Resolved(resolved)) = receiver.arrive(frame, case, now_ms) {
                    judge_payload(&resolved.payload);
                    if resolved
                        .payload
                        .verify_transaction_and_payload(TEST_NETWORK_ID)
                    {
                        let _admitted =
                            receiver.admit_verified(&resolved.payload, resolved.encoding, now_ms);
                    }
                }
            }
            Ok(LinkFrame::Control(LinkControl::Announce(session))) => {
                let _unavailable = receiver.announce(session, now_ms);
            }
            Ok(LinkFrame::Control(LinkControl::Unknown(digest))) => {
                let _unavailable = receiver.unknown(digest, now_ms);
            }
            Ok(LinkFrame::Control(LinkControl::Request(_) | LinkControl::Known(_))) | Err(_) => {}
        }
        while let Ok(Some(_)) = receiver.release_next(now_ms) {}
        if generator.one_in(7) {
            let _swept = receiver.sweep(now_ms + LINK_HOLD_TIMEOUT_MS + 1);
        }
        assert!(receiver.held_len() <= LINK_HOLD_CAPACITY);
        assert!(receiver.known_len() <= REFERENCED_TABLE_CAPACITY);
    }
}

/// One valid link frame: a payload with generated slot encodings, or a control frame. An
/// announcement names a session some earlier referenced frame awaits when there is one.
fn generated_link_frame(
    generator: &mut DecodeBoundaryGenerator,
    referenced: &mut Vec<Delegation>,
) -> Option<Vec<u8>> {
    let payload = generated_payload(generator)?;
    let sessions = payload.delegations();
    let wire = match generator.usize(5) {
        0 => {
            let announced = match referenced.len() {
                0 => sessions.origin.clone(),
                len => referenced[generator.usize(len)].clone(),
            };
            LinkControl::Announce(announced).to_wire()
        }
        1 => LinkControl::Unknown(sessions.origin.digest().ok()?).to_wire(),
        2 => LinkControl::Request(sessions.origin.digest().ok()?).to_wire(),
        3 => match generator.usize(2) {
            0 => LinkControl::Known(sessions.origin.digest().ok()?).to_wire(),
            _ => payload.to_wire(),
        },
        _ => {
            let references = PerSlot {
                origin: DelegationRef::Digest(sessions.origin.digest().ok()?),
                hop: DelegationRef::Digest(sessions.hop.digest().ok()?),
            };
            referenced.push(sessions.origin.clone());
            if referenced.len() > LINK_HOLD_CAPACITY {
                referenced.remove(0);
            }
            WirePayload::view(&payload, references).to_wire()
        }
    };
    wire.ok().map(|wire| wire.to_vec())
}

fn exercise_message_decode_boundary(data: &[u8]) {
    if let Ok(payload) = MessagePayload::from_wire(data) {
        judge_payload(&payload);
    }
    let text = String::from_utf8_lossy(data);
    let encoded = Encoded::from_encoded_str(&text);
    let _bytes = encoded.decode::<Vec<u8>>();
    if let Ok(payload) = encoded.decode::<MessagePayload>() {
        judge_payload(&payload);
    }
}

fn exercise_payload_wire_forms(payload: &MessagePayload) {
    if let Ok(wire) = payload.to_wire() {
        exercise_message_decode_boundary(&wire);
    }
    if let Ok(encoded) = payload.encode() {
        let text = encoded.to_string();
        exercise_message_decode_boundary(text.as_bytes());
    }
}

fn judge_payload(payload: &MessagePayload) {
    let _verified = payload.verify(TEST_NETWORK_ID);
    let _message = payload.transaction.data::<Message>();
}

fn generated_payload(generator: &mut DecodeBoundaryGenerator) -> Option<MessagePayload> {
    let session = DelegateeKey::new_with_seckey(&generator.secret_key()).ok()?;
    let peer: Did = generator.secret_key().address().into();
    MessagePayload::new_send(
        Message::custom(&generator.bytes(256)).ok()?,
        MessageSigner::new(&session, TEST_NETWORK_ID),
        peer,
        peer,
    )
    .ok()
}

fn generated_reassembly_limits() -> ReassemblyLimits {
    ReassemblyLimits {
        max_pending_messages: 4,
        max_chunk_data_len: 128,
        max_message_bytes: 512,
        max_chunks_per_message: 16,
        max_total_buffered_cost: 4096,
        slot_overhead: 8,
        max_completed_ids: 8,
    }
}

fn generated_case_count() -> usize {
    std::env::var(CASES_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|cases| cases.clamp(1, 50_000))
        .unwrap_or(DEFAULT_CASES)
}

struct DecodeBoundaryGenerator {
    seed: u64,
    state: u64,
}

impl DecodeBoundaryGenerator {
    fn named(label: &str) -> Self {
        let seed = std::env::var(SEED_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(time_seed);
        let state = fold_label(seed, label);
        Self { seed, state }
    }

    fn seed(&self) -> u64 {
        self.seed
    }

    fn next_u64(&mut self) -> u64 {
        self.state = splitmix64(self.state);
        self.state
    }

    fn one_in(&mut self, modulus: u64) -> bool {
        self.next_u64().is_multiple_of(modulus.max(1))
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.usize(max_len.saturating_add(1));
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push(self.byte());
        }
        bytes
    }

    /// `bytes`, unchanged half the time; otherwise truncated at a generated length or with one
    /// generated byte replaced, so both well-formed and damaged frames reach the decoder.
    fn mutated(&mut self, mut bytes: Vec<u8>) -> Vec<u8> {
        match self.usize(4) {
            0 => bytes.truncate(self.usize(bytes.len())),
            1 => {
                let position = self.usize(bytes.len());
                let replacement = self.byte();
                if let Some(byte) = bytes.get_mut(position) {
                    *byte = replacement;
                }
            }
            _ => {}
        }
        bytes
    }

    fn byte(&mut self) -> u8 {
        u8::try_from(self.next_u64() & 0xff).unwrap_or(0)
    }

    fn usize(&mut self, upper: usize) -> usize {
        let upper = u64::try_from(upper).unwrap_or(u64::MAX).max(1);
        usize::try_from(self.next_u64() % upper).unwrap_or(0)
    }

    fn secret_key(&mut self) -> SecretKey {
        loop {
            let candidate = hex::encode(self.array_32());
            if let Ok(key) = SecretKey::try_from(candidate.as_str()) {
                return key;
            }
        }
    }

    fn array_16(&mut self) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        self.fill(&mut bytes);
        bytes
    }

    fn array_32(&mut self) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        self.fill(&mut bytes);
        bytes
    }

    fn fill(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.byte();
        }
    }

    fn chunk(&mut self, now: u128) -> Chunk {
        let total = self.total();
        Chunk {
            chunk: [self.position(total), total],
            data: Bytes::from(self.bytes(256)),
            meta: ChunkMeta {
                id: Uuid::from_bytes(self.array_16()),
                ts_ms: self.timestamp(now),
                ttl_ms: self.ttl_ms(),
            },
        }
    }

    fn total(&mut self) -> usize {
        match self.usize(5) {
            0 => 0,
            1 => usize::MAX,
            _ => self.usize(32).saturating_add(1),
        }
    }

    fn position(&mut self, total: usize) -> usize {
        match self.usize(4) {
            0 => total,
            1 => usize::MAX,
            _ => self.usize(total.saturating_add(4)),
        }
    }

    fn timestamp(&mut self, now: u128) -> u128 {
        match self.usize(4) {
            0 => now,
            1 => now.saturating_sub(u128::from(DEFAULT_TTL_MS).saturating_add(1)),
            2 => now.saturating_add(60_000),
            _ => now.saturating_sub(u128::from(self.next_u64() % 10_000)),
        }
    }

    fn ttl_ms(&mut self) -> u64 {
        match self.usize(4) {
            0 => 0,
            1 => DEFAULT_TTL_MS,
            2 => u64::MAX,
            _ => self.next_u64() % 10_000,
        }
    }
}

fn time_seed() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(duration.as_secs());
    nanos ^ u64::from(std::process::id())
}

fn fold_label(seed: u64, label: &str) -> u64 {
    label
        .bytes()
        .fold(seed, |state, byte| splitmix64(state ^ u64::from(byte)))
}

fn splitmix64(mut state: u64) -> u64 {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}
