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
use crate::dht::Did;
use crate::ecc::SecretKey;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::MessageVerificationExt;
use crate::session::SessionSk;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

const SEED_ENV: &str = "RINGS_DECODE_BOUNDARY_SEED";
const CASES_ENV: &str = "RINGS_DECODE_BOUNDARY_CASES";
const DEFAULT_CASES: usize = 128;

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
    let session = SessionSk::new_with_seckey(&generator.secret_key()).ok()?;
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
