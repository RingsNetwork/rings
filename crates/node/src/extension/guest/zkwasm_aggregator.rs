//! Browser-capable verifier for Delphinus zkWasm aggregator receipts.
//!
//! The wire shape matches the g1024/Delphinus service output: `proof`,
//! `verify_instance`, and `aux` are little-endian 32-byte `uint256` chunks, while
//! `target_instances` is the matrix supplied to `AggregatorVerifier.verify`.
//! Verification ports the generated Solidity verifier into Rust over BN254.
//! Rings binds a receipt by requiring the guest circuit to expose
//! [`zkwasm_aggregator_claim_scalar`] as the first target instance.

#[path = "zkwasm_aggregator_generated.rs"]
mod zkwasm_aggregator_generated;

use ark_bn254::Bn254;
use ark_bn254::Fq;
use ark_bn254::Fq2;
use ark_bn254::Fr;
use ark_bn254::G1Affine;
use ark_bn254::G1Projective;
use ark_bn254::G2Affine;
use ark_ec::pairing::Pairing;
use ark_ec::AffineRepr;
use ark_ec::CurveGroup;
use ark_ff::BigInteger;
use ark_ff::One;
use ark_ff::PrimeField;
use ark_ff::Zero;
use bytes::Bytes;
use num_bigint::BigUint;
use rings_core::ecc::keccak256;
use serde::Deserialize;
use serde::Serialize;

use super::GuestError;
use super::GuestReceipt;
use super::GuestReceiptClaim;
use super::GuestReceiptVerifier;

type Word = BigUint;

const WORD_BYTES: usize = 32;
const PAIRING_WORDS: usize = 12;
const VERIFY_BUF_WORDS: usize = 52;
const CHALLENGE_WORDS: usize = 152;
const CLAIM_BINDING_DOMAIN: &[u8] = b"rings:guest:wasm:zkwasm:aggregator:claim:v1";
const P_MOD_BE: [u8; WORD_BYTES] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x97, 0x81, 0x6a, 0x91, 0x68, 0x71, 0xca, 0x8d, 0x3c, 0x20, 0x8c, 0x16, 0xd8, 0x7c, 0xfd, 0x47,
];
const Q_MOD_BE: [u8; WORD_BYTES] = [
    0x30, 0x64, 0x4e, 0x72, 0xe1, 0x31, 0xa0, 0x29, 0xb8, 0x50, 0x45, 0xb6, 0x81, 0x81, 0x58, 0x5d,
    0x28, 0x33, 0xe8, 0x48, 0x79, 0xb9, 0x70, 0x91, 0x43, 0xe1, 0xf5, 0x93, 0xf0, 0x00, 0x00, 0x01,
];

/// Delphinus zkWasm aggregator proof payload carried in [`GuestReceipt::proof`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZkWasmAggregatorReceipt {
    /// Aggregated proof transcript as little-endian 32-byte chunks.
    pub proof: Vec<[u8; WORD_BYTES]>,
    /// Aggregator verifier instance as little-endian 32-byte chunks.
    pub verify_instance: Vec<[u8; WORD_BYTES]>,
    /// Auxiliary division witnesses as little-endian 32-byte chunks.
    pub aux: Vec<[u8; WORD_BYTES]>,
    /// Target instances passed to the generated aggregator verifier.
    pub target_instances: Vec<Vec<[u8; WORD_BYTES]>>,
}

impl ZkWasmAggregatorReceipt {
    /// Build a payload from g1024 service byte arrays.
    pub fn from_service_bytes(
        proof: &[u8],
        verify_instance: &[u8],
        aux: &[u8],
        target_instances: Vec<Vec<[u8; WORD_BYTES]>>,
    ) -> Result<Self, GuestError> {
        Ok(Self {
            proof: split_chunks(proof, "proof")?,
            verify_instance: split_chunks(verify_instance, "verify_instance")?,
            aux: split_chunks(aux, "aux")?,
            target_instances,
        })
    }

    /// Encode this payload for [`GuestReceipt::proof`].
    pub fn encode(&self) -> Result<Bytes, GuestError> {
        bincode::serialize(self)
            .map(Bytes::from)
            .map_err(|error| GuestError::ProofDataEncode {
                reason: error.to_string(),
            })
    }

    /// Decode this payload from [`GuestReceipt::proof`].
    pub fn decode(bytes: &[u8]) -> Result<Self, GuestError> {
        bincode::deserialize(bytes).map_err(|error| GuestError::ProofDataDecode {
            reason: error.to_string(),
        })
    }
}

/// Verifier for Delphinus zkWasm aggregator receipts.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZkWasmAggregatorVerifier;

impl ZkWasmAggregatorVerifier {
    /// Build a zkWasm aggregator verifier.
    pub fn new() -> Self {
        Self
    }
}

impl GuestReceiptVerifier for ZkWasmAggregatorVerifier {
    fn verify(&self, claim: &GuestReceiptClaim, receipt: &GuestReceipt) -> Result<(), GuestError> {
        if !claim.matches_receipt(receipt) {
            return Err(GuestError::ReceiptClaimMismatch);
        }
        let payload = ZkWasmAggregatorReceipt::decode(receipt.proof.as_ref())?;
        verify_claim_binding(claim, &payload)?;
        verify_aggregator_payload(&payload)
    }
}

/// Compute the first target instance that binds a zkWasm proof to a Rings receipt claim.
pub fn zkwasm_aggregator_claim_scalar(claim: &GuestReceiptClaim) -> [u8; WORD_BYTES] {
    word_to_32_le(&zkwasm_aggregator_claim_word(claim))
}

fn zkwasm_aggregator_claim_word(claim: &GuestReceiptClaim) -> Word {
    let mut transcript = Vec::new();
    transcript.extend_from_slice(CLAIM_BINDING_DOMAIN);
    transcript.extend_from_slice(claim.program_hash.as_bytes());
    append_len_prefixed(&mut transcript, claim.public_input.bytes());
    append_len_prefixed(&mut transcript, claim.public_output.bytes());
    let hash = keccak256(transcript.as_slice());
    BigUint::from_bytes_be(&hash) % q_mod()
}

fn verify_claim_binding(
    claim: &GuestReceiptClaim,
    payload: &ZkWasmAggregatorReceipt,
) -> Result<(), GuestError> {
    let Some(first_row) = payload.target_instances.first() else {
        return Err(GuestError::ReceiptClaimMismatch);
    };
    let Some(first_instance) = first_row.first() else {
        return Err(GuestError::ReceiptClaimMismatch);
    };
    if chunk_to_word_le(first_instance) != zkwasm_aggregator_claim_word(claim) {
        return Err(GuestError::ReceiptClaimMismatch);
    }
    Ok(())
}

fn verify_aggregator_payload(payload: &ZkWasmAggregatorReceipt) -> Result<(), GuestError> {
    let proof = chunks_to_words(payload.proof.as_slice());
    let verify_instance = chunks_to_words(payload.verify_instance.as_slice());
    let aux = chunks_to_words(payload.aux.as_slice());
    let target_instances = payload
        .target_instances
        .iter()
        .map(|row| chunks_to_words(row.as_slice()))
        .collect::<Vec<_>>();
    verify_aggregator(&proof, &verify_instance, &aux, &target_instances)
}

fn verify_aggregator(
    proof: &[Word],
    verify_instance: &[Word],
    aux: &[Word],
    target_instances: &[Vec<Word>],
) -> Result<(), GuestError> {
    let mut buf = vec![zero(); VERIFY_BUF_WORDS];
    let mut len = 0usize;
    for row in target_instances {
        for value in row {
            set_word(buf.as_mut_slice(), len, value.clone())?;
            len = len
                .checked_add(1)
                .ok_or_else(|| proof_decode("instance length overflow"))?;
        }
    }
    for value in verify_instance {
        set_word(buf.as_mut_slice(), len, value.clone())?;
        len = len
            .checked_add(1)
            .ok_or_else(|| proof_decode("instance length overflow"))?;
    }
    let instance_hash = hash_instances(buf.as_slice(), len)?;
    set_word(buf.as_mut_slice(), 2, instance_hash)?;

    calc_verify_circuit_lagrange(buf.as_mut_slice())?;
    get_challenges(proof, buf.as_mut_slice())?;
    zkwasm_aggregator_generated::step1(proof, aux, buf.as_mut_slice())?;
    zkwasm_aggregator_generated::step2(proof, aux, buf.as_mut_slice())?;
    zkwasm_aggregator_generated::step3(proof, aux, buf.as_mut_slice())?;
    let ret = zkwasm_aggregator_generated::step4(proof, aux, buf.as_mut_slice())?;

    if ret.iter().any(is_zero_word) {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "invalid generated pairing point".to_string(),
        });
    }
    let mut pairing_buf = vec![zero(); PAIRING_WORDS];
    set_word(pairing_buf.as_mut_slice(), 0, word(ret.as_slice(), 0)?)?;
    set_word(pairing_buf.as_mut_slice(), 1, word(ret.as_slice(), 1)?)?;
    set_word(pairing_buf.as_mut_slice(), 6, word(ret.as_slice(), 2)?)?;
    set_word(pairing_buf.as_mut_slice(), 7, word(ret.as_slice(), 3)?)?;
    fill_verify_circuits_g2(pairing_buf.as_mut_slice())?;
    if !pairing(pairing_buf.as_slice())? {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "verify circuit pairing check failed".to_string(),
        });
    }
    Ok(())
}

fn append_len_prefixed(buffer: &mut Vec<u8>, value: &[u8]) {
    buffer.extend_from_slice(&(value.len() as u64).to_be_bytes());
    buffer.extend_from_slice(value);
}

fn split_chunks(bytes: &[u8], field: &'static str) -> Result<Vec<[u8; WORD_BYTES]>, GuestError> {
    if bytes.len() % WORD_BYTES != 0 {
        return Err(GuestError::ProofDataDecode {
            reason: format!("{field} length is not a multiple of {WORD_BYTES} bytes"),
        });
    }
    bytes
        .chunks_exact(WORD_BYTES)
        .map(|chunk| {
            let mut word = [0u8; WORD_BYTES];
            word.copy_from_slice(chunk);
            Ok(word)
        })
        .collect()
}

fn chunks_to_words(chunks: &[[u8; WORD_BYTES]]) -> Vec<Word> {
    chunks.iter().map(chunk_to_word_le).collect()
}

fn chunk_to_word_le(chunk: &[u8; WORD_BYTES]) -> Word {
    BigUint::from_bytes_le(chunk)
}

fn word_to_32_le(value: &Word) -> [u8; WORD_BYTES] {
    let mut out = [0u8; WORD_BYTES];
    let bytes = value.to_bytes_le();
    for (slot, byte) in out.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    out
}

fn word_to_32_be(value: &Word) -> Result<[u8; WORD_BYTES], GuestError> {
    let bytes = value.to_bytes_be();
    if bytes.len() > WORD_BYTES {
        return Err(proof_decode("uint256 word exceeds 32 bytes"));
    }
    let mut out = [0u8; WORD_BYTES];
    let offset = WORD_BYTES.saturating_sub(bytes.len());
    for (slot, byte) in out.iter_mut().skip(offset).zip(bytes.iter()) {
        *slot = *byte;
    }
    Ok(out)
}

fn q_mod() -> Word {
    BigUint::from_bytes_be(&Q_MOD_BE)
}

fn p_mod() -> Word {
    BigUint::from_bytes_be(&P_MOD_BE)
}

fn one() -> Word {
    Word::from(1u8)
}

fn zero() -> Word {
    Word::from(0u8)
}

fn is_zero_word(value: &Word) -> bool {
    value == &zero()
}

fn word_dec(value: &str) -> Result<Word, GuestError> {
    BigUint::parse_bytes(value.as_bytes(), 10).ok_or_else(|| GuestError::ProofProgramInvalid {
        reason: format!("invalid generated verifier constant {value}"),
    })
}

fn word(values: &[Word], index: usize) -> Result<Word, GuestError> {
    values
        .get(index)
        .cloned()
        .ok_or_else(|| proof_decode(format!("missing word at index {index}")))
}

fn set_word(values: &mut [Word], index: usize, value: Word) -> Result<(), GuestError> {
    let Some(slot) = values.get_mut(index) else {
        return Err(proof_decode(format!("word index {index} is out of range")));
    };
    *slot = value;
    Ok(())
}

fn evm_addmod(a: Word, b: Word, modulus: Word) -> Word {
    (a + b) % modulus
}

fn evm_mulmod(a: Word, b: Word, modulus: Word) -> Word {
    (a * b) % modulus
}

fn q_mod_minus(value: Word) -> Result<Word, GuestError> {
    let modulus = q_mod();
    if value > modulus {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "field subtraction underflow in generated verifier".to_string(),
        });
    }
    Ok(modulus - value)
}

fn fr_div(a: Word, b: Word, aux: Word) -> Result<Word, GuestError> {
    if is_zero_word(&b) {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "division by zero in generated verifier".to_string(),
        });
    }
    let modulus = q_mod();
    if evm_mulmod(b, aux.clone(), modulus.clone()) != a % modulus.clone() {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "division witness mismatch in generated verifier".to_string(),
        });
    }
    Ok(aux % modulus)
}

fn fr_pow(a: Word, power: Word) -> Word {
    a.modpow(&power, &q_mod())
}

fn hash_instances(values: &[Word], len: usize) -> Result<Word, GuestError> {
    let words = values
        .get(..len)
        .ok_or_else(|| proof_decode(format!("hash_instances length {len} is out of range")))?;
    let mut bytes = Vec::with_capacity(words.len().saturating_mul(WORD_BYTES));
    for value in words {
        bytes.extend_from_slice(&word_to_32_be(value)?);
    }
    Ok(BigUint::from_bytes_be(&keccak256(bytes.as_slice())) % q_mod())
}

fn hash_with_trailing_zero(values: &[Word], len: usize) -> Result<Word, GuestError> {
    let words = values
        .get(..len)
        .ok_or_else(|| proof_decode(format!("challenge length {len} is out of range")))?;
    let mut bytes = Vec::with_capacity(words.len().saturating_mul(WORD_BYTES).saturating_add(1));
    for value in words {
        bytes.extend_from_slice(&word_to_32_be(value)?);
    }
    bytes.push(0);
    Ok(BigUint::from_bytes_be(&keccak256(bytes.as_slice())))
}

fn squeeze_challenge(absorbing: &mut [Word], len: usize) -> Result<Word, GuestError> {
    set_word(absorbing, len, zero())?;
    let challenge = hash_with_trailing_zero(absorbing, len)?;
    set_word(absorbing, 0, challenge.clone())?;
    Ok(challenge % q_mod())
}

fn check_on_curve(x: &Word, y: &Word) -> Result<(), GuestError> {
    if is_zero_word(x) && is_zero_word(y) {
        return Ok(());
    }
    let modulus = p_mod();
    let left = evm_mulmod(y.clone(), y.clone(), modulus.clone());
    let x2 = evm_mulmod(x.clone(), x.clone(), modulus.clone());
    let x3 = evm_mulmod(x2, x.clone(), modulus.clone());
    let right = evm_addmod(x3, word_dec("3")?, modulus);
    if left != right {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "generated verifier point is not on BN254 G1".to_string(),
        });
    }
    Ok(())
}

fn fill_verify_circuits_g2(pairing_buf: &mut [Word]) -> Result<(), GuestError> {
    set_word(
        pairing_buf,
        2,
        word_dec("17131510004222863239408580011965663790707790083758508980521431927481455316244")?,
    )?;
    set_word(
        pairing_buf,
        3,
        word_dec("13239981408604951437450014900617239305783482703680168225708776375178235958414")?,
    )?;
    set_word(
        pairing_buf,
        4,
        word_dec("16758014440826914722508163669181150884980044788823938544831532724103579389691")?,
    )?;
    set_word(
        pairing_buf,
        5,
        word_dec("8824020152601776802611073397094798788346180506242534327221018398726195908688")?,
    )?;
    set_word(
        pairing_buf,
        8,
        word_dec("11559732032986387107991004021392285783925812861821192530917403151452391805634")?,
    )?;
    set_word(
        pairing_buf,
        9,
        word_dec("10857046999023057135944570762232829481370756359578518086990519993285655852781")?,
    )?;
    set_word(
        pairing_buf,
        10,
        word_dec("17805874995975841540914202342111839520379459829704422454583296818431106115052")?,
    )?;
    set_word(
        pairing_buf,
        11,
        word_dec("13392588948715843804641432497768002650278120570034223513918757245338268106653")?,
    )?;
    Ok(())
}

fn calc_verify_circuit_lagrange(buf: &mut [Word]) -> Result<(), GuestError> {
    set_word(
        buf,
        0,
        word_dec("4246485553913621569067470645392660895027649716862194263429156289599464730996")?,
    )?;
    set_word(
        buf,
        1,
        word_dec("15899192751216009222363664025367168439943024377694320324116625506830866071849")?,
    )?;
    msm(buf, 0, 1)
}

fn get_challenges(transcript: &[Word], buf: &mut [Word]) -> Result<(), GuestError> {
    let mut absorbing = vec![zero(); CHALLENGE_WORDS];
    set_word(
        absorbing.as_mut_slice(),
        0,
        word_dec("6901874450049050949560638117932163566866099697656126185212198417536751485015")?,
    )?;
    set_word(absorbing.as_mut_slice(), 1, word(buf, 0)?)?;
    set_word(absorbing.as_mut_slice(), 2, word(buf, 1)?)?;

    let mut pos = 3usize;
    let mut transcript_pos = 0usize;
    absorb_curve_points(
        transcript,
        absorbing.as_mut_slice(),
        &mut pos,
        &mut transcript_pos,
        9,
    )?;
    set_word(buf, 2, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    absorb_curve_points(
        transcript,
        absorbing.as_mut_slice(),
        &mut pos,
        &mut transcript_pos,
        2,
    )?;
    set_word(buf, 3, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    set_word(buf, 4, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    absorb_curve_points(
        transcript,
        absorbing.as_mut_slice(),
        &mut pos,
        &mut transcript_pos,
        9,
    )?;
    set_word(buf, 5, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    absorb_curve_points(
        transcript,
        absorbing.as_mut_slice(),
        &mut pos,
        &mut transcript_pos,
        3,
    )?;
    set_word(buf, 6, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    for _ in 0..76 {
        set_word(
            absorbing.as_mut_slice(),
            pos,
            word(transcript, transcript_pos)?,
        )?;
        pos = pos
            .checked_add(1)
            .ok_or_else(|| proof_decode("challenge position overflow"))?;
        transcript_pos = transcript_pos
            .checked_add(1)
            .ok_or_else(|| proof_decode("transcript position overflow"))?;
    }
    set_word(buf, 7, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;

    pos = 1;
    set_word(buf, 8, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;
    absorb_curve_points(
        transcript,
        absorbing.as_mut_slice(),
        &mut pos,
        &mut transcript_pos,
        1,
    )?;
    set_word(buf, 9, squeeze_challenge(absorbing.as_mut_slice(), pos)?)?;
    let x = word(transcript, transcript_pos)?;
    let y = word(
        transcript,
        transcript_pos
            .checked_add(1)
            .ok_or_else(|| proof_decode("transcript position overflow"))?,
    )?;
    check_on_curve(&x, &y)
}

fn absorb_curve_points(
    transcript: &[Word],
    absorbing: &mut [Word],
    pos: &mut usize,
    transcript_pos: &mut usize,
    count: usize,
) -> Result<(), GuestError> {
    for _ in 0..count {
        let x = word(transcript, *transcript_pos)?;
        let y = word(
            transcript,
            transcript_pos
                .checked_add(1)
                .ok_or_else(|| proof_decode("transcript position overflow"))?,
        )?;
        check_on_curve(&x, &y)?;
        set_word(absorbing, *pos, x)?;
        *pos = pos
            .checked_add(1)
            .ok_or_else(|| proof_decode("challenge position overflow"))?;
        set_word(absorbing, *pos, y)?;
        *pos = pos
            .checked_add(1)
            .ok_or_else(|| proof_decode("challenge position overflow"))?;
        *transcript_pos = transcript_pos
            .checked_add(2)
            .ok_or_else(|| proof_decode("transcript position overflow"))?;
    }
    Ok(())
}

fn msm(buf: &mut [Word], offset: usize, count: usize) -> Result<(), GuestError> {
    if count != 1 {
        return Err(GuestError::ProofProgramInvalid {
            reason: format!("unsupported generated verifier MSM count {count}"),
        });
    }
    let x = word(buf, offset)?;
    let y = word(
        buf,
        offset
            .checked_add(1)
            .ok_or_else(|| proof_decode("point offset overflow"))?,
    )?;
    let scalar = word(
        buf,
        offset
            .checked_add(2)
            .ok_or_else(|| proof_decode("scalar offset overflow"))?,
    )?;
    let result = g1_mul(&x, &y, &scalar)?;
    set_word(buf, offset, result.0)?;
    set_word(
        buf,
        offset
            .checked_add(1)
            .ok_or_else(|| proof_decode("point offset overflow"))?,
        result.1,
    )
}

fn ecc_mul(buf: &mut [Word], offset: usize) -> Result<(), GuestError> {
    let scalar = word(
        buf,
        offset
            .checked_add(2)
            .ok_or_else(|| proof_decode("scalar offset overflow"))?,
    )?;
    if scalar == one() {
        return Ok(());
    }
    msm(buf, offset, 1)
}

fn ecc_mul_add(buf: &mut [Word], offset: usize) -> Result<(), GuestError> {
    let acc = g1_from_words(
        &word(buf, offset)?,
        &word(
            buf,
            offset
                .checked_add(1)
                .ok_or_else(|| proof_decode("point offset overflow"))?,
        )?,
    )?;
    let point_offset = offset
        .checked_add(2)
        .ok_or_else(|| proof_decode("point offset overflow"))?;
    let scalar_offset = offset
        .checked_add(4)
        .ok_or_else(|| proof_decode("scalar offset overflow"))?;
    let scaled = g1_mul_projective(
        &word(buf, point_offset)?,
        &word(
            buf,
            point_offset
                .checked_add(1)
                .ok_or_else(|| proof_decode("point offset overflow"))?,
        )?,
        &word(buf, scalar_offset)?,
    )?;
    let sum = acc.into_group() + scaled;
    let result = g1_projective_to_words(sum)?;
    set_word(buf, offset, result.0)?;
    set_word(
        buf,
        offset
            .checked_add(1)
            .ok_or_else(|| proof_decode("point offset overflow"))?,
        result.1,
    )
}

fn g1_mul(x: &Word, y: &Word, scalar: &Word) -> Result<(Word, Word), GuestError> {
    g1_projective_to_words(g1_mul_projective(x, y, scalar)?)
}

fn g1_mul_projective(x: &Word, y: &Word, scalar: &Word) -> Result<G1Projective, GuestError> {
    let point = g1_from_words(x, y)?;
    if point.is_zero() || is_zero_word(scalar) {
        return Ok(G1Projective::zero());
    }
    Ok(point * fr_from_word_mod(scalar)?)
}

fn g1_from_words(x: &Word, y: &Word) -> Result<G1Affine, GuestError> {
    if is_zero_word(x) && is_zero_word(y) {
        return Ok(G1Affine::identity());
    }
    let point = G1Affine::new_unchecked(fq_from_word(x)?, fq_from_word(y)?);
    validate_g1(point)?;
    Ok(point)
}

fn validate_g1(point: G1Affine) -> Result<(), GuestError> {
    if !point.is_on_curve() || !point.is_in_correct_subgroup_assuming_on_curve() {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "invalid BN254 G1 point".to_string(),
        });
    }
    Ok(())
}

fn g2_from_words(
    x_im: &Word,
    x_re: &Word,
    y_im: &Word,
    y_re: &Word,
) -> Result<G2Affine, GuestError> {
    let point = G2Affine::new_unchecked(
        Fq2::new(fq_from_word(x_re)?, fq_from_word(x_im)?),
        Fq2::new(fq_from_word(y_re)?, fq_from_word(y_im)?),
    );
    if !point.is_on_curve() || !point.is_in_correct_subgroup_assuming_on_curve() {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "invalid BN254 G2 point".to_string(),
        });
    }
    Ok(point)
}

fn g1_projective_to_words(point: G1Projective) -> Result<(Word, Word), GuestError> {
    if point.is_zero() {
        return Ok((zero(), zero()));
    }
    let affine = point.into_affine();
    Ok((field_to_word(&affine.x), field_to_word(&affine.y)))
}

fn pairing(input: &[Word]) -> Result<bool, GuestError> {
    if input.len() != PAIRING_WORDS {
        return Err(proof_decode("pairing input must have 12 words"));
    }
    let g1_a = g1_from_words(&word(input, 0)?, &word(input, 1)?)?;
    let g2_a = g2_from_words(
        &word(input, 2)?,
        &word(input, 3)?,
        &word(input, 4)?,
        &word(input, 5)?,
    )?;
    let g1_b = g1_from_words(&word(input, 6)?, &word(input, 7)?)?;
    let g2_b = g2_from_words(
        &word(input, 8)?,
        &word(input, 9)?,
        &word(input, 10)?,
        &word(input, 11)?,
    )?;
    Ok(
        Bn254::multi_pairing([g1_a, g1_b], [g2_a, g2_b]).0
            == <Bn254 as Pairing>::TargetField::one(),
    )
}

fn fq_from_word(value: &Word) -> Result<Fq, GuestError> {
    if value >= &p_mod() {
        return Err(GuestError::ReceiptVerificationFailed {
            reason: "BN254 base-field element is out of range".to_string(),
        });
    }
    Ok(Fq::from_be_bytes_mod_order(&word_to_32_be(value)?))
}

fn fr_from_word_mod(value: &Word) -> Result<Fr, GuestError> {
    Ok(Fr::from_be_bytes_mod_order(&word_to_32_be(
        &(value % q_mod()),
    )?))
}

fn field_to_word<F>(value: &F) -> Word
where F: PrimeField {
    BigUint::from_bytes_be(&value.into_bigint().to_bytes_be())
}

fn proof_decode(reason: impl Into<String>) -> GuestError {
    GuestError::ProofDataDecode {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::guest::GuestProgramHash;
    use crate::extension::guest::GuestPublicInput;
    use crate::extension::guest::GuestPublicOutput;

    fn hash(seed: u8) -> GuestProgramHash {
        match GuestProgramHash::new([seed; WORD_BYTES], "test") {
            Ok(hash) => hash,
            Err(error) => panic!("valid test hash failed: {error}"),
        }
    }

    fn claim() -> GuestReceiptClaim {
        GuestReceiptClaim::new(
            hash(7),
            GuestPublicInput::new(Bytes::from_static(b"in")),
            GuestPublicOutput::new(Bytes::from_static(b"out")),
        )
    }

    fn receipt_from_payload(
        claim: &GuestReceiptClaim,
        payload: ZkWasmAggregatorReceipt,
    ) -> GuestReceipt {
        let proof = match payload.encode() {
            Ok(bytes) => bytes,
            Err(error) => panic!("encode failed: {error}"),
        };
        GuestReceipt {
            program_hash: claim.program_hash,
            public_input: claim.public_input.clone(),
            public_output: claim.public_output.clone(),
            proof,
        }
    }

    fn malformed_bound_receipt(claim: &GuestReceiptClaim) -> GuestReceipt {
        receipt_from_payload(claim, ZkWasmAggregatorReceipt {
            proof: Vec::new(),
            verify_instance: Vec::new(),
            aux: Vec::new(),
            target_instances: vec![vec![zkwasm_aggregator_claim_scalar(claim)]],
        })
    }

    #[test]
    fn zkwasm_claim_scalar_is_stable_and_field_reduced() {
        let scalar = zkwasm_aggregator_claim_scalar(&claim());
        let word = chunk_to_word_le(&scalar);

        assert!(word < q_mod());
        assert_eq!(scalar, [
            0x91, 0x61, 0xc8, 0x65, 0x63, 0x84, 0xc1, 0xc7, 0x16, 0x15, 0x8b, 0x79, 0xd3, 0xdb,
            0xcc, 0xf2, 0xa7, 0x99, 0x00, 0x2a, 0xdf, 0xec, 0x4f, 0x42, 0x81, 0x8e, 0x68, 0x2a,
            0x37, 0x9f, 0x14, 0x14,
        ]);
    }

    #[test]
    fn service_bytes_must_be_whole_words() {
        assert!(matches!(
            ZkWasmAggregatorReceipt::from_service_bytes(&[1, 2], &[], &[], Vec::new()),
            Err(GuestError::ProofDataDecode { .. })
        ));
    }

    #[test]
    fn generated_field_subtraction_underflow_returns_error() {
        assert!(matches!(
            q_mod_minus(q_mod() + one()),
            Err(GuestError::ReceiptVerificationFailed { .. })
        ));
    }

    #[test]
    fn verifier_rejects_receipt_without_claim_binding() {
        let claim = claim();
        let payload = ZkWasmAggregatorReceipt {
            proof: Vec::new(),
            verify_instance: Vec::new(),
            aux: Vec::new(),
            target_instances: Vec::new(),
        };
        let receipt = receipt_from_payload(&claim, payload);

        assert_eq!(
            ZkWasmAggregatorVerifier::new().verify(&claim, &receipt),
            Err(GuestError::ReceiptClaimMismatch)
        );
    }

    #[test]
    fn verifier_rejects_malformed_proof_after_claim_binding() {
        let claim = claim();
        let receipt = malformed_bound_receipt(&claim);

        assert!(matches!(
            ZkWasmAggregatorVerifier::new().verify(&claim, &receipt),
            Err(GuestError::ProofDataDecode { .. })
        ));
    }

    #[cfg(target_arch = "wasm32")]
    mod wasm32 {
        use wasm_bindgen_test::wasm_bindgen_test;
        use wasm_bindgen_test::wasm_bindgen_test_configure;

        use super::*;

        wasm_bindgen_test_configure!(run_in_browser);

        #[wasm_bindgen_test]
        fn zkwasm_aggregator_wasm32_rejects_malformed_bound_receipt() {
            let claim = claim();
            let receipt = malformed_bound_receipt(&claim);

            assert!(matches!(
                ZkWasmAggregatorVerifier::new().verify(&claim, &receipt),
                Err(GuestError::ProofDataDecode { .. })
            ));
        }
    }
}
