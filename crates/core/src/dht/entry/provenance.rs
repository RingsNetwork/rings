//! Data-topic write provenance and fair per-writer element retention (#1179, #1220).
//!
//! Write operations and replicated sync states on [`EntryKind::Data`](super::EntryKind::Data)
//! carriers are classified through algebraic sum types ([`DataProvenance`], [`RetractionWitness`],
//! [`SignedDataMutation`]) and higher-order continuation combinators (`with_live_and_then`,
//! `and_then`, `then_some`), isolating per-actor provenance proofs from storage transitions.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use super::ElementDigest;
use super::Entry;
use super::EntryDot;
use super::EntryKind;
use super::EntryOperation;
use super::EntryVersion;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::delegation::DelegateeKey;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;
use crate::message::DomainTag;
use crate::message::MessageSigner;
use crate::message::MessageVerification;
use crate::message::Principal;
use crate::message::SigningDomain;

/// Domain separation tag for per-author Swarm data mutation signatures (#1220).
pub(crate) const DATA_MUTATION_DOMAIN_TAG: DomainTag =
    crate::domain_tag!("rings-core:data-entry:mutation");

/// Key identifying which per-actor CRDT slot a [`SignedDataMutation`] attests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum DataProofKey {
    /// Attests the reset floor `co_registers[actor]`.
    Register(Did),
    /// Attests the element add `co_values[value][actor]` where `digest = keccak256(value)`.
    Add(ElementDigest, Did),
    /// Attests the element removal `co_removes[digest][actor]`.
    Remove(ElementDigest, Did),
}

impl DataProofKey {
    pub(crate) const fn actor(self) -> Did {
        match self {
            Self::Register(actor) | Self::Add(_, actor) | Self::Remove(_, actor) => actor,
        }
    }

    pub(crate) const fn is_element_add(self) -> bool {
        matches!(self, Self::Add(_, _))
    }
}

/// Operation kind attested by a [`DataMutationClaim`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum DataMutationOpKind {
    Reset,
    Append,
    Retract,
}

/// Fixed-size Merkle batch claim signed inside a [`SignedDataMutation`] (#1220).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct DataMutationClaim {
    pub(crate) did: Did,
    pub(crate) kind: DataMutationOpKind,
    pub(crate) version: EntryVersion,
    pub(crate) leaf_count: u32,
    pub(crate) merkle_root: [u8; 32],
}

fn merkle_leaf_element(digest: [u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 33];
    buf[0] = 0x01;
    buf[1..33].copy_from_slice(&digest);
    keccak256(&buf)
}

fn merkle_leaf_dot(dot: EntryDot) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = 0x02;
    buf[1..17].copy_from_slice(&dot.version.logical_time_ms.to_be_bytes());
    buf[17..21].copy_from_slice(&dot.version.counter.to_be_bytes());
    buf[21..41].copy_from_slice(dot.version.actor.as_bytes());
    buf[41..61].copy_from_slice(dot.version.operation.as_bytes());
    buf[61..65].copy_from_slice(&dot.index.to_be_bytes());
    keccak256(&buf)
}

fn merkle_parent_hash(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = 0x03;
    buf[1..33].copy_from_slice(&left);
    buf[33..65].copy_from_slice(&right);
    keccak256(&buf)
}

fn compute_merkle_tree(leaves: &[[u8; 32]]) -> ([u8; 32], Vec<Vec<[u8; 32]>>) {
    if leaves.is_empty() {
        return ([0u8; 32], Vec::new());
    }
    let mut layers = vec![leaves.to_vec()];
    while let Some(curr) = layers.last() {
        if curr.len() <= 1 {
            break;
        }
        let next = curr
            .chunks(2)
            .filter_map(|pair| match pair {
                [left, right] => Some(merkle_parent_hash(*left, *right)),
                [only] => Some(*only),
                _ => None,
            })
            .collect::<Vec<_>>();
        layers.push(next);
    }
    let root = layers
        .last()
        .and_then(|top| top.first().copied())
        .unwrap_or([0u8; 32]);
    let non_root_layers = layers.len().saturating_sub(1);
    let mut branches = Vec::with_capacity(leaves.len());
    for leaf_idx in 0..leaves.len() {
        let mut idx = leaf_idx;
        let mut siblings = Vec::with_capacity(non_root_layers);
        for layer in layers.iter().take(non_root_layers) {
            if let Some(&sibling) = layer.get(idx ^ 1) {
                siblings.push(sibling);
            }
            idx /= 2;
        }
        branches.push(siblings);
    }
    (root, branches)
}

fn verify_merkle_inclusion(
    leaf_hash: [u8; 32],
    leaf_index: u32,
    leaf_count: u32,
    siblings: &[[u8; 32]],
    expected_root: [u8; 32],
) -> bool {
    if leaf_count == 0 || leaf_index >= leaf_count {
        return false;
    }
    let mut idx = leaf_index as usize;
    let mut len = leaf_count as usize;
    let mut current = leaf_hash;
    let mut sib_iter = siblings.iter().copied();
    while len > 1 {
        let sibling_idx = idx ^ 1;
        if sibling_idx < len {
            let Some(sibling) = sib_iter.next() else {
                return false;
            };
            current = if idx & 1 == 0 {
                merkle_parent_hash(current, sibling)
            } else {
                merkle_parent_hash(sibling, current)
            };
        }
        idx /= 2;
        len = len.div_ceil(2);
    }
    sib_iter.next().is_none() && current == expected_root
}

/// Cryptographic author attestation over a Swarm-originated [`EntryKind::Data`] mutation (#1220).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDataMutation {
    pub(crate) key: DataProofKey,
    pub(crate) claim: DataMutationClaim,
    pub(crate) leaf_index: u32,
    pub(crate) witness_dot: Option<EntryDot>,
    pub(crate) merkle_siblings: Vec<[u8; 32]>,
    pub(crate) proof: Option<MessageVerification>,
}

#[derive(Serialize, Deserialize)]
struct WireProofBundle {
    batches: Vec<(DataMutationClaim, Option<MessageVerification>)>,
    items: Vec<WireProofItem>,
}

#[derive(Serialize, Deserialize)]
struct WireProofItem {
    key: DataProofKey,
    batch_index: u32,
    leaf_index: u32,
    witness_dot: Option<EntryDot>,
    merkle_siblings: Vec<[u8; 32]>,
}

pub(super) fn encode_proof_bundle(proofs: &[SignedDataMutation]) -> Result<Vec<u8>> {
    let mut batches: Vec<(DataMutationClaim, Option<MessageVerification>)> = Vec::new();
    let mut items = Vec::with_capacity(proofs.len());
    for mutation in proofs {
        let batch_pos = batches
            .iter()
            .position(|(c, p)| *c == mutation.claim && *p == mutation.proof)
            .unwrap_or_else(|| {
                let idx = batches.len();
                batches.push((mutation.claim, mutation.proof.clone()));
                idx
            });
        let batch_index = u32::try_from(batch_pos).unwrap_or(u32::MAX);
        items.push(WireProofItem {
            key: mutation.key,
            batch_index,
            leaf_index: mutation.leaf_index,
            witness_dot: mutation.witness_dot,
            merkle_siblings: mutation.merkle_siblings.clone(),
        });
    }
    rings_codec::serialize(&WireProofBundle { batches, items }).map_err(Error::CodecSerialize)
}

pub(super) fn decode_proof_bundle(raw: &[u8]) -> Result<Vec<SignedDataMutation>> {
    let bundle =
        rings_codec::deserialize::<WireProofBundle>(raw).map_err(Error::CodecDeserialize)?;
    if bundle.batches.is_empty() || bundle.items.is_empty() {
        return Err(Error::DataEntryWriteUnauthorized);
    }
    let mut out = Vec::with_capacity(bundle.items.len());
    for item in bundle.items {
        let Some((claim, proof)) = bundle.batches.get(item.batch_index as usize) else {
            return Err(Error::DataEntryWriteUnauthorized);
        };
        if proof.is_none() {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        out.push(SignedDataMutation {
            key: item.key,
            claim: *claim,
            leaf_index: item.leaf_index,
            witness_dot: item.witness_dot,
            merkle_siblings: item.merkle_siblings,
            proof: proof.clone(),
        });
    }
    Ok(out)
}

impl PartialOrd for SignedDataMutation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SignedDataMutation {
    fn cmp(&self, other: &Self) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }
        self.key
            .cmp(&other.key)
            .then_with(|| self.proof.is_some().cmp(&other.proof.is_some()))
            .then_with(|| self.effective_version().cmp(&other.effective_version()))
            .then_with(|| self.witness_dot.cmp(&other.witness_dot))
            .then_with(|| self.claim.cmp(&other.claim))
            .then_with(|| {
                self.proof
                    .as_ref()
                    .map(|p| p.ts_ms)
                    .cmp(&other.proof.as_ref().map(|p| p.ts_ms))
            })
            .then_with(|| self.leaf_index.cmp(&other.leaf_index))
            .then_with(|| self.merkle_siblings.cmp(&other.merkle_siblings))
    }
}

impl SignedDataMutation {
    fn effective_version(&self) -> EntryVersion {
        self.witness_dot.map_or(self.claim.version, |d| d.version)
    }

    pub(crate) fn is_corrupted(&self) -> bool {
        self.proof.is_none()
    }

    pub(crate) fn matches_register(&self, floor: EntryVersion) -> bool {
        self.proof.is_some()
            && self.key == DataProofKey::Register(floor.actor)
            && self.claim.kind == DataMutationOpKind::Reset
            && self.claim.version == floor
    }

    pub(crate) fn matches_add(&self, digest: ElementDigest, dot: EntryDot) -> bool {
        self.proof.is_some()
            && self.key == DataProofKey::Add(digest, dot.version.actor)
            && matches!(
                self.claim.kind,
                DataMutationOpKind::Reset | DataMutationOpKind::Append
            )
            && self.witness_dot.is_none()
            && self.claim.version == dot.version
            && verify_merkle_inclusion(
                merkle_leaf_element(digest.0),
                self.leaf_index,
                self.claim.leaf_count,
                &self.merkle_siblings,
                self.claim.merkle_root,
            )
    }

    pub(crate) fn matches_remove(&self, digest: ElementDigest, remove_dot: EntryDot) -> bool {
        let actor = remove_dot.version.actor;
        if self.proof.is_none()
            || self.key != DataProofKey::Remove(digest, actor)
            || self.claim.kind != DataMutationOpKind::Retract
        {
            return false;
        }
        match self.witness_dot {
            Some(dot) => {
                dot == remove_dot
                    && verify_merkle_inclusion(
                        merkle_leaf_dot(dot),
                        self.leaf_index,
                        self.claim.leaf_count,
                        &self.merkle_siblings,
                        self.claim.merkle_root,
                    )
            }
            None => {
                !self.claim.version.actor.is_default()
                    && remove_dot.version.actor == self.claim.version.actor
                    && (remove_dot.version.logical_time_ms < self.claim.version.logical_time_ms
                        || (remove_dot.version.logical_time_ms
                            == self.claim.version.logical_time_ms
                            && remove_dot.version.counter
                                <= self.claim.version.counter.saturating_add(64)))
                    && verify_merkle_inclusion(
                        merkle_leaf_element(digest.0),
                        self.leaf_index,
                        self.claim.leaf_count,
                        &self.merkle_siblings,
                        self.claim.merkle_root,
                    )
            }
        }
    }

    fn verify_claim_structure(&self, carrier: &Entry) -> Result<()> {
        if self.claim.did != carrier.did {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        let actor = self.key.actor();
        if actor.is_default() {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        let claim_matches = match (&self.key, self.claim.kind) {
            (DataProofKey::Register(key_actor), DataMutationOpKind::Reset) => {
                *key_actor == self.claim.version.actor
                    && self.witness_dot.is_none()
                    && self.merkle_siblings.is_empty()
                    && (self.claim.leaf_count > 0 || self.claim.merkle_root == [0u8; 32])
                    && carrier.register_floors().any(|f| f == self.claim.version)
            }
            (
                DataProofKey::Add(digest, key_actor),
                DataMutationOpKind::Reset | DataMutationOpKind::Append,
            ) => {
                *key_actor == self.claim.version.actor
                    && self.witness_dot.is_none()
                    && verify_merkle_inclusion(
                        merkle_leaf_element(digest.0),
                        self.leaf_index,
                        self.claim.leaf_count,
                        &self.merkle_siblings,
                        self.claim.merkle_root,
                    )
                    && carrier.attributed_elements().any(|(value, dot)| {
                        dot.version == self.claim.version && keccak256(value.as_ref()) == digest.0
                    })
            }
            (DataProofKey::Remove(digest, key_actor), DataMutationOpKind::Retract) => {
                *key_actor == actor
                    && match self.witness_dot {
                        Some(dot) => {
                            dot.version.actor == actor
                                && verify_merkle_inclusion(
                                    merkle_leaf_dot(dot),
                                    self.leaf_index,
                                    self.claim.leaf_count,
                                    &self.merkle_siblings,
                                    self.claim.merkle_root,
                                )
                                && (carrier
                                    .crdt
                                    .tombstones
                                    .iter()
                                    .any(|t| t.element == *digest && t.dot == dot)
                                    || (carrier.crdt.tombstones.is_empty()
                                        && *digest == ElementDigest([0u8; 32])
                                        && carrier.crdt.dots.contains(&dot)))
                        }
                        None => {
                            !self.claim.version.actor.is_default()
                                && self.claim.version.actor == actor
                                && verify_merkle_inclusion(
                                    merkle_leaf_element(digest.0),
                                    self.leaf_index,
                                    self.claim.leaf_count,
                                    &self.merkle_siblings,
                                    self.claim.merkle_root,
                                )
                                && (carrier.crdt.tombstones.iter().any(|t| {
                                    t.element == *digest && self.matches_remove(t.element, t.dot)
                                }) || (carrier.crdt.tombstones.is_empty()
                                    && carrier.crdt.dots.is_empty()
                                    && carrier
                                        .data
                                        .iter()
                                        .any(|v| keccak256(v.as_ref()) == digest.0)))
                        }
                    }
            }
            _ => false,
        };
        claim_matches
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
    }
}

/// The selector by which a [`EntryOperation::Tombstone`] names the elements to retract from a
/// carrier: either explicit CRDT add dots (`Dots`) when a write witness is present, or raw payload
/// values (`Values`) otherwise.
pub(super) enum RetractionWitness<'a> {
    /// Retract only elements held at one of the specified add dots.
    Dots(BTreeSet<EntryDot>),
    /// Retract elements whose payload bytes match one of the specified values.
    Values(BTreeSet<&'a Bytes>),
}

impl<'a> RetractionWitness<'a> {
    /// Construct the retraction witness carried by `entry`.
    pub(super) fn of(entry: &'a Entry) -> Self {
        entry
            .crdt
            .has_write_witness()
            .then(|| {
                (!entry.crdt.dots.is_empty())
                    .then(|| Self::Dots(entry.crdt.dots.iter().copied().collect()))
            })
            .flatten()
            .unwrap_or_else(|| Self::Values(entry.data.iter().collect()))
    }

    /// Whether the held element `(value, dot)` is selected by this witness.
    pub(super) fn matches(&self, value: &Bytes, dot: &EntryDot) -> bool {
        match self {
            Self::Dots(dots) => dots.contains(dot),
            Self::Values(values) => values.contains(value),
        }
    }
}

/// Algebraic classification of a verified data-topic write claim issued by `writer`.
enum DataProvenance<'a> {
    /// Full-carrier reset (`Overwrite`) authored by `writer`.
    Reset { entry: &'a Entry, writer: Did },
    /// Element-set append (`Extend`) authored by `writer`.
    Append { entry: &'a Entry, writer: Did },
    /// Element retraction (`Tombstone`) authored by `writer` under `witness`.
    Retract {
        witness: RetractionWitness<'a>,
        writer: Did,
    },
}

impl<'a> DataProvenance<'a> {
    /// Construct a provenance proof for `(op, writer, introducer)`, or reject with
    /// [`Error::DataEntryWriteUnauthorized`].
    fn try_new(op: &'a EntryOperation, writer: Did, introducer: Principal) -> Result<Self> {
        (!writer.is_default()
            && op.entry().is_exclusively_authored_by(writer)
            && op.entry().crdt.co_dots.is_empty()
            && op.entry().crdt.co_registers.is_empty())
        .then_some(())
        .ok_or(Error::DataEntryWriteUnauthorized)
        .and_then(|()| match op {
            EntryOperation::Overwrite(entry) => (entry.crdt.register.is_some()
                && entry.crdt.dots.len() == entry.data.len()
                && entry.permits_reset_envelope_from(introducer))
            .then_some(Self::Reset { entry, writer })
            .ok_or(Error::DataEntryWriteUnauthorized),
            EntryOperation::Extend(entry) => (entry.crdt.dots.len() == entry.data.len()
                && entry.has_delta_envelope())
            .then_some(Self::Append { entry, writer })
            .ok_or(Error::DataEntryWriteUnauthorized),
            EntryOperation::Tombstone(entry) => entry
                .has_delta_envelope()
                .then(|| Self::Retract {
                    witness: RetractionWitness::of(entry),
                    writer,
                })
                .ok_or(Error::DataEntryWriteUnauthorized),
        })
    }

    /// Construct a provenance claim for validating `op` against a live local carrier.
    fn for_local(op: &'a EntryOperation, writer: Did) -> Self {
        match op {
            EntryOperation::Overwrite(entry) => Self::Reset { entry, writer },
            EntryOperation::Extend(entry) => Self::Append { entry, writer },
            EntryOperation::Tombstone(entry) => Self::Retract {
                witness: RetractionWitness::of(entry),
                writer,
            },
        }
    }

    /// Verify that this write claim may be applied to `live`.
    fn verify_on_live(&self, live: &Entry) -> Result<()> {
        match self {
            Self::Reset { entry, writer } => live.permits_reset_by(entry, *writer),
            Self::Append { entry, writer } => live.permits_append_by(entry, *writer),
            Self::Retract { witness, writer } => live.permits_retraction_by(witness, *writer),
        }
    }
}

impl EntryVersion {
    /// Whether this version is explicitly attributed to `writer` (`!actor.is_default() && actor == writer`).
    pub(super) fn is_authored_by(self, writer: Did) -> bool {
        !self.actor.is_default() && self.actor == writer
    }
}

impl EntryDot {
    /// Whether this dot carries a non-default author identity (`!version.actor.is_default()`).
    pub(super) fn is_attributed(self) -> bool {
        !self.version.actor.is_default()
    }

    /// Return the attributed author of this dot, or `None` when unattributed.
    fn attributed_actor(self) -> Option<Did> {
        self.is_attributed().then_some(self.version.actor)
    }
}

impl EntryOperation {
    /// Attach cryptographic author proofs signed by `signer` (`actor`) to a stamped `Data`
    /// operation before it is sent or applied by `Swarm` (#1220).
    pub(crate) fn with_author_proofs(
        self,
        actor: Did,
        signer: MessageSigner<impl Borrow<DelegateeKey>>,
        now_ms: u128,
    ) -> Result<Self> {
        if self.kind() != EntryKind::Data || actor.is_default() {
            return Ok(self);
        }
        match self {
            EntryOperation::Overwrite(mut entry) => {
                if let Some(version) = entry.crdt.register {
                    let elements = entry
                        .data
                        .iter()
                        .map(|value| keccak256(value.as_ref()))
                        .collect::<Vec<_>>();
                    let leaves = elements
                        .iter()
                        .map(|&elem| merkle_leaf_element(elem))
                        .collect::<Vec<_>>();
                    let (merkle_root, branches) = compute_merkle_tree(&leaves);
                    let leaf_count = u32::try_from(elements.len()).map_err(|_| {
                        Error::EntryDotIndexOutOfBounds {
                            index: elements.len(),
                        }
                    })?;
                    let claim = DataMutationClaim {
                        did: entry.did,
                        kind: DataMutationOpKind::Reset,
                        version,
                        leaf_count,
                        merkle_root,
                    };
                    let bytes = rings_codec::serialize(&claim).map_err(Error::CodecSerialize)?;
                    let sig = signer.sign_at(DATA_MUTATION_DOMAIN_TAG, &bytes, now_ms)?;
                    let mut proofs = Vec::with_capacity(elements.len().saturating_add(1));
                    proofs.push(SignedDataMutation {
                        key: DataProofKey::Register(actor),
                        claim,
                        leaf_index: 0,
                        witness_dot: None,
                        merkle_siblings: Vec::new(),
                        proof: Some(sig.clone()),
                    });
                    for (idx, (elem, merkle_siblings)) in
                        elements.into_iter().zip(branches).enumerate()
                    {
                        proofs.push(SignedDataMutation {
                            key: DataProofKey::Add(ElementDigest(elem), actor),
                            claim,
                            leaf_index: u32::try_from(idx).unwrap_or(u32::MAX),
                            witness_dot: None,
                            merkle_siblings,
                            proof: Some(sig.clone()),
                        });
                    }
                    entry.crdt.proofs = proofs;
                }
                Ok(EntryOperation::Overwrite(entry))
            }
            EntryOperation::Extend(mut entry) => {
                if let Some(first_dot) = entry.crdt.dots.first().copied() {
                    let elements = entry
                        .data
                        .iter()
                        .map(|value| keccak256(value.as_ref()))
                        .collect::<Vec<_>>();
                    let leaves = elements
                        .iter()
                        .map(|&elem| merkle_leaf_element(elem))
                        .collect::<Vec<_>>();
                    let (merkle_root, branches) = compute_merkle_tree(&leaves);
                    let leaf_count = u32::try_from(elements.len()).map_err(|_| {
                        Error::EntryDotIndexOutOfBounds {
                            index: elements.len(),
                        }
                    })?;
                    let claim = DataMutationClaim {
                        did: entry.did,
                        kind: DataMutationOpKind::Append,
                        version: first_dot.version,
                        leaf_count,
                        merkle_root,
                    };
                    let bytes = rings_codec::serialize(&claim).map_err(Error::CodecSerialize)?;
                    let sig = signer.sign_at(DATA_MUTATION_DOMAIN_TAG, &bytes, now_ms)?;
                    entry.crdt.proofs = elements
                        .into_iter()
                        .zip(branches)
                        .enumerate()
                        .map(|(idx, (elem, merkle_siblings))| SignedDataMutation {
                            key: DataProofKey::Add(ElementDigest(elem), actor),
                            claim,
                            leaf_index: u32::try_from(idx).unwrap_or(u32::MAX),
                            witness_dot: None,
                            merkle_siblings,
                            proof: Some(sig.clone()),
                        })
                        .collect();
                }
                Ok(EntryOperation::Extend(entry))
            }
            EntryOperation::Tombstone(mut entry) => {
                if entry.data.is_empty() {
                    let dots = entry.crdt.dots.clone();
                    let leaves = dots
                        .iter()
                        .map(|&dot| merkle_leaf_dot(dot))
                        .collect::<Vec<_>>();
                    let (merkle_root, branches) = compute_merkle_tree(&leaves);
                    let leaf_count = u32::try_from(dots.len())
                        .map_err(|_| Error::EntryDotIndexOutOfBounds { index: dots.len() })?;
                    let version = dots
                        .iter()
                        .map(|dot| dot.version)
                        .max()
                        .unwrap_or_else(|| EntryVersion::new(now_ms, actor, Did::default()));
                    let claim = DataMutationClaim {
                        did: entry.did,
                        kind: DataMutationOpKind::Retract,
                        version,
                        leaf_count,
                        merkle_root,
                    };
                    let bytes = rings_codec::serialize(&claim).map_err(Error::CodecSerialize)?;
                    let sig = signer.sign_at(DATA_MUTATION_DOMAIN_TAG, &bytes, now_ms)?;
                    entry.crdt.proofs = dots
                        .into_iter()
                        .zip(branches)
                        .enumerate()
                        .map(|(idx, (dot, merkle_siblings))| SignedDataMutation {
                            key: DataProofKey::Remove(ElementDigest([0u8; 32]), actor),
                            claim,
                            leaf_index: u32::try_from(idx).unwrap_or(u32::MAX),
                            witness_dot: Some(dot),
                            merkle_siblings,
                            proof: Some(sig.clone()),
                        })
                        .collect();
                } else {
                    let elements = entry
                        .data
                        .iter()
                        .map(|value| keccak256(value.as_ref()))
                        .collect::<Vec<_>>();
                    let leaves = elements
                        .iter()
                        .map(|&elem| merkle_leaf_element(elem))
                        .collect::<Vec<_>>();
                    let (merkle_root, branches) = compute_merkle_tree(&leaves);
                    let leaf_count = u32::try_from(elements.len()).map_err(|_| {
                        Error::EntryDotIndexOutOfBounds {
                            index: elements.len(),
                        }
                    })?;
                    let op_digest = entry.operation_digest_for(actor, &entry.data)?;
                    let version = EntryVersion::new(now_ms, actor, op_digest);
                    let claim = DataMutationClaim {
                        did: entry.did,
                        kind: DataMutationOpKind::Retract,
                        version,
                        leaf_count,
                        merkle_root,
                    };
                    let bytes = rings_codec::serialize(&claim).map_err(Error::CodecSerialize)?;
                    let sig = signer.sign_at(DATA_MUTATION_DOMAIN_TAG, &bytes, now_ms)?;
                    entry.crdt.proofs = elements
                        .into_iter()
                        .zip(branches)
                        .enumerate()
                        .map(|(idx, (elem, merkle_siblings))| SignedDataMutation {
                            key: DataProofKey::Remove(ElementDigest(elem), actor),
                            claim,
                            leaf_index: u32::try_from(idx).unwrap_or(u32::MAX),
                            witness_dot: None,
                            merkle_siblings,
                            proof: Some(sig.clone()),
                        })
                        .collect();
                }
                Ok(EntryOperation::Tombstone(entry))
            }
        }
    }

    /// Validate a data-topic write operation `self` issued by `writer` and introduced by
    /// `introducer` at `now_ms` inside `network_id` (#1179, #1220).
    pub(crate) fn validate_data_write(
        &self,
        now_ms: u128,
        network_id: u32,
        writer: Did,
        introducer: Principal,
    ) -> Result<()> {
        self.validate_admissible_at(now_ms, network_id)
            .and_then(|()| DataProvenance::try_new(self, writer, introducer).map(drop))
    }

    /// Validate a data-topic operation `self` issued by `writer` against the live carrier
    /// `local` currently held at the storage slot (#1179, #1220).
    pub(crate) fn validate_data_write_on_local(
        &self,
        local: Option<&Entry>,
        writer: Did,
    ) -> Result<()> {
        if writer.is_default() {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        Entry::with_live_and_then(local, |live| {
            DataProvenance::for_local(self, writer).verify_on_live(live)
        })
    }
}

impl Entry {
    /// Evaluate `then` on `local` when a live carrier is present, or succeed with `Ok(())` when the
    /// slot is empty.
    pub(crate) fn with_live_and_then(
        local: Option<&Self>,
        then: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        local.map_or(Ok(()), then)
    }

    /// Iterate over all reset floors carried by `self` (`register` and `co_registers`).
    pub(super) fn register_floors(&self) -> impl Iterator<Item = EntryVersion> + '_ {
        self.crdt
            .register
            .into_iter()
            .chain(self.crdt.co_registers.iter().copied())
    }

    /// Verify any cryptographic [`SignedDataMutation`] proofs carried on `self`, deduplicating
    /// signature checks across shared Merkle batch claims (#1220).
    pub(super) fn validate_data_proofs(&self, now_ms: u128, network_id: u32) -> Result<()> {
        if self.crdt.proofs.is_empty() {
            return Ok(());
        }
        let domain = SigningDomain::new(DATA_MUTATION_DOMAIN_TAG, network_id);
        let max_ts = now_ms.saturating_add(TS_OFFSET_TOLERANCE_MS);
        let mut verified_batches: Vec<(Did, DataMutationClaim, MessageVerification)> = Vec::new();
        for item in &self.crdt.proofs {
            let Some(proof) = &item.proof else {
                return Err(Error::DataEntryWriteUnauthorized);
            };
            if proof.ts_ms > max_ts || item.claim.version.logical_time_ms > max_ts {
                return Err(Error::EntryVersionAheadOfClock);
            }
            let actor = item.key.actor();
            item.verify_claim_structure(self)?;
            let already_verified = verified_batches
                .iter()
                .any(|(a, c, p)| *a == actor && *c == item.claim && p == proof);
            if !already_verified {
                let data = rings_codec::serialize(&item.claim).map_err(Error::CodecSerialize)?;
                if !proof.verify_live_at(domain, &data, proof.ts_ms)
                    || proof.delegation.delegator_did() != actor
                {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                verified_batches.push((actor, item.claim, proof.clone()));
            }
        }
        Ok(())
    }

    /// Whether `self` carries at least one verified cryptographic proof for `actor`.
    fn has_signed_proof_for(&self, actor: Did) -> bool {
        self.crdt
            .proofs
            .iter()
            .any(|proof| !proof.is_corrupted() && proof.key.actor() == actor)
    }

    fn has_signed_register_proof(&self, floor: EntryVersion) -> bool {
        self.crdt
            .proofs
            .iter()
            .any(|proof| proof.matches_register(floor))
    }

    fn has_signed_remove_proof(&self, element: ElementDigest, remove_dot: EntryDot) -> bool {
        self.crdt
            .proofs
            .iter()
            .any(|proof| proof.matches_remove(element, remove_dot))
    }

    /// Whether every version and proof carried by `self` is explicitly authored by `writer`.
    pub(super) fn is_exclusively_authored_by(&self, writer: Did) -> bool {
        !writer.is_default()
            && self
                .versions()
                .all(|version| version.is_authored_by(writer))
            && self
                .crdt
                .proofs
                .iter()
                .all(|proof| !proof.is_corrupted() && proof.key.actor() == writer)
    }

    /// Whether an `Overwrite` carrier satisfies the reset envelope invariant for `introducer`:
    /// carries no raw tombstones and, when introduced by a remote peer, is non-empty.
    fn permits_reset_envelope_from(&self, introducer: Principal) -> bool {
        self.crdt.tombstones.is_empty() && (!self.data.is_empty() || introducer == Principal::Local)
    }

    /// Whether an `Extend` or `Tombstone` carrier satisfies the delta envelope invariant: carries
    /// neither a reset register nor raw tombstones.
    fn has_delta_envelope(&self) -> bool {
        self.crdt.register.is_none()
            && self.crdt.co_registers.is_empty()
            && self.crdt.tombstones.is_empty()
    }

    /// Verify that `incoming` may reset this live carrier on behalf of `writer`.
    fn permits_reset_by(&self, incoming: &Self, writer: Did) -> Result<()> {
        (!incoming.data.is_empty() && self.is_exclusively_authored_by(writer))
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
    }

    /// Verify that `incoming` may append elements on behalf of `writer`, checking authorship
    /// against `writer`'s own actor slot in `co_values[value]` so co-authors are never locked out
    /// (#1220).
    fn permits_append_by(&self, incoming: &Self, writer: Did) -> Result<()> {
        self.topic_buffer().and_then(|buffer| {
            incoming
                .data
                .iter()
                .all(|value| {
                    let unattributed_only = buffer
                        .values
                        .get(value)
                        .is_some_and(|dot| !dot.is_attributed())
                        && !buffer.co_values.contains_key(value);
                    !unattributed_only
                        && buffer
                            .co_values
                            .get(value)
                            .and_then(|actors| actors.get(&writer))
                            .is_none_or(|held_dot| held_dot.version.is_authored_by(writer))
                })
                .then_some(())
                .ok_or(Error::DataEntryWriteUnauthorized)
        })
    }

    /// Verify that `witness` retracts only `writer`'s own actor slot in `co_values[value]`
    /// (#1220).
    fn permits_retraction_by(&self, witness: &RetractionWitness<'_>, writer: Did) -> Result<()> {
        self.topic_buffer().and_then(|buffer| {
            let permitted = match witness {
                RetractionWitness::Values(values) => values.iter().all(|value| {
                    !buffer.values.contains_key(*value)
                        || buffer
                            .co_values
                            .get(*value)
                            .and_then(|actors| actors.get(&writer))
                            .is_some_and(|held_dot| held_dot.version.is_authored_by(writer))
                }),
                RetractionWitness::Dots(dots) => {
                    dots.iter().all(|dot| dot.version.is_authored_by(writer))
                        && buffer.values.keys().all(|value| {
                            buffer
                                .dots_for(value)
                                .filter(|held_dot| dots.contains(held_dot))
                                .all(|held_dot| held_dot.version.is_authored_by(writer))
                        })
                }
            };
            permitted
                .then_some(())
                .ok_or(Error::DataEntryWriteUnauthorized)
        })
    }

    /// Validate a synced or fetched data carrier `self` against the live carrier `local` currently
    /// held at the slot (#1179, #1220).
    ///
    /// Enforces:
    /// - Every version in `self` is explicitly attributed (`!actor.is_default() && !operation.is_default()`),
    ///   `dots.len() == data.len()`, `co_dots` indices are in bounds, and no active element dot is
    ///   reused in `tombstones`.
    /// - Per-actor reset floors are self-consistent (`v.actor == floor.actor => v >= floor`).
    /// - Cross-actor floors and tombstones merge commutatively without rejection because
    ///   `DataTopicBuffer` scopes floors and tombstones by `actor`.
    /// - Same-actor floors and tombstones retiring `live`'s state must carry `actor`'s
    ///   [`SignedDataMutation`] whenever `live` holds Swarm-signed state for `actor`, and must
    ///   carry a consistent operation witness otherwise.
    pub(crate) fn validate_data_sync_on_local(&self, local: Option<&Self>) -> Result<()> {
        self.validate_data_sync_structure()
            .and_then(|()| self.permits_synced_register_on(local))
            .and_then(|()| {
                Self::with_live_and_then(local, |live| self.permits_synced_tombstones_on(live))
            })
    }

    fn validate_data_sync_structure(&self) -> Result<()> {
        if self.crdt.dots.len() != self.data.len() {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        if self.versions().any(|version| version.actor.is_default()) {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        if self
            .crdt
            .co_dots
            .iter()
            .any(|&(index, _)| (index as usize) >= self.data.len())
        {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        if self.crdt.register.is_none() && !self.crdt.co_registers.is_empty() {
            return Err(Error::DataEntryWriteUnauthorized);
        }
        if let Some(max_reg) = self.crdt.register {
            if self.crdt.co_registers.iter().any(|&reg| reg >= max_reg) {
                return Err(Error::DataEntryWriteUnauthorized);
            }
        }
        let mut seen_floor_actors = BTreeSet::new();
        for floor in self.register_floors() {
            if !seen_floor_actors.insert(floor.actor) {
                return Err(Error::DataEntryWriteUnauthorized);
            }
        }
        Ok(())
    }

    fn floor_witness_valid_on_live(&self, floor: EntryVersion, live: &Self) -> bool {
        if live.has_signed_proof_for(floor.actor) {
            return self.has_signed_register_proof(floor);
        }
        let retires_live_same_actor = live
            .versions()
            .any(|version| version.actor == floor.actor && version < floor);
        if !retires_live_same_actor {
            return true;
        }
        let floor_elements = self
            .data
            .iter()
            .enumerate()
            .filter(|&(index, _)| {
                self.crdt
                    .dots
                    .get(index)
                    .is_some_and(|d| d.version == floor)
                    || self
                        .crdt
                        .co_dots
                        .iter()
                        .any(|&(co_idx, d)| co_idx as usize == index && d.version == floor)
            })
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        if floor_elements.is_empty() {
            floor.operation != floor.actor
        } else {
            let live_uses_synthetic_op = live
                .versions()
                .any(|v| v.actor == floor.actor && v.operation == floor.actor);
            live_uses_synthetic_op
                || self
                    .operation_digest_for(floor.actor, &floor_elements)
                    .is_ok_and(|digest| floor.operation == digest)
        }
    }

    /// Verify that every per-actor reset floor on `self` is self-consistent and authenticated
    /// against `local`.
    fn permits_synced_register_on(&self, local: Option<&Self>) -> Result<()> {
        let valid = self.register_floors().all(|floor| {
            (!self.has_signed_proof_for(floor.actor) || self.has_signed_register_proof(floor))
                && self
                    .versions()
                    .filter(|version| version.actor == floor.actor)
                    .all(|version| version >= floor)
                && local.is_none_or(|live| self.floor_witness_valid_on_live(floor, live))
        });
        valid.then_some(()).ok_or(Error::DataEntryWriteUnauthorized)
    }

    /// Verify that any same-actor tombstone on `self` covering a live element in `live` carries an
    /// authentic removal witness for that actor.
    fn permits_synced_tombstones_on(&self, live: &Self) -> Result<()> {
        if self.crdt.tombstones.is_empty() {
            return Ok(());
        }
        let mut by_element_actor = BTreeMap::<(ElementDigest, Did), EntryDot>::new();
        for tombstone in &self.crdt.tombstones {
            let actor = tombstone.dot.version.actor;
            if (self.has_signed_proof_for(actor) || live.has_signed_proof_for(actor))
                && !self.has_signed_remove_proof(tombstone.element, tombstone.dot)
            {
                return Err(Error::DataEntryWriteUnauthorized);
            }
            let key = (tombstone.element, actor);
            if by_element_actor
                .get(&key)
                .is_none_or(|current| current < &tombstone.dot)
            {
                by_element_actor.insert(key, tombstone.dot);
            }
        }
        live.attributed_elements()
            .all(|(value, held_dot)| {
                let actor = held_dot.version.actor;
                let digest = ElementDigest(keccak256(value.as_ref()));
                let Some(&remove_dot) = by_element_actor.get(&(digest, actor)) else {
                    return true;
                };
                if held_dot > remove_dot {
                    return true;
                }
                if live.has_signed_proof_for(actor) {
                    return self.has_signed_remove_proof(digest, remove_dot);
                }
                if remove_dot == held_dot {
                    return true;
                }
                let live_uses_synthetic_op = held_dot.version.operation == actor;
                live_uses_synthetic_op
                    || self
                        .operation_digest_for(actor, std::slice::from_ref(value))
                        .is_ok_and(|expected| remove_dot.version.operation == expected)
            })
            .then_some(())
            .ok_or(Error::DataEntryWriteUnauthorized)
    }

    /// Iterate over `(value, dot)` pairs in `self` (both primary and co-author dots) whose dot
    /// carries a non-default author.
    fn attributed_elements(&self) -> impl Iterator<Item = (&Bytes, EntryDot)> + '_ {
        self.data
            .iter()
            .zip(self.crdt.dots.iter().copied())
            .chain(self.crdt.co_dots.iter().filter_map(|&(index, dot)| {
                self.data.get(index as usize).map(|value| (value, dot))
            }))
            .filter(|(_, dot)| dot.is_attributed())
    }

    /// Return `self` unchanged: tombstones on `EntryKind::Data` are actor-scoped in
    /// [`DataTopicBuffer`](super::DataTopicBuffer), so cross-actor tombstones never cover another
    /// writer's dots and are never evicted (#1220).
    pub(crate) fn without_cross_actor_tombstones_for(
        self,
        _incoming: &Self,
        _fallback_actor: Did,
    ) -> Self {
        self
    }
}

/// Materialize normalized data-topic elements under the fair per-writer cap `max_len`,
/// returning `(data, dots, co_dots)` (#1179, #1220).
///
/// Eviction ([`retain_fair_data_elements`]) triggers only when unique `Bytes` values exceed
/// `max_len`, and advances water-filling until `max_len` unique `Bytes` values are admitted so
/// co-author dots neither consume extra `max_len` slots nor regress a value's primary timestamp
/// at or below capacity, while preserving semilattice associativity across co-authored floods.
pub(super) fn materialize_data_elements(
    values: BTreeMap<Bytes, EntryDot>,
    mut co_values: BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
    max_len: usize,
) -> (Vec<Bytes>, Vec<EntryDot>, Vec<(u32, EntryDot)>) {
    if max_len == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    if values.len() <= max_len {
        let mut visible = values.into_iter().collect::<Vec<_>>();
        visible.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
            left_dot
                .cmp(right_dot)
                .then_with(|| left_value.cmp(right_value))
        });
        let mut data = Vec::with_capacity(visible.len());
        let mut dots = Vec::with_capacity(visible.len());
        let mut co_dots = Vec::new();
        for (index, (value, primary_dot)) in visible.into_iter().enumerate() {
            if let Some(actors) = co_values.remove(&value) {
                if let Ok(index_u32) = u32::try_from(index) {
                    let mut extra_dots = actors
                        .into_values()
                        .filter(|&dot| dot != primary_dot)
                        .collect::<Vec<_>>();
                    extra_dots.sort();
                    co_dots.extend(extra_dots.into_iter().map(|dot| (index_u32, dot)));
                }
            }
            data.push(value);
            dots.push(primary_dot);
        }
        return (data, dots, co_dots);
    }

    let mut attributions = Vec::with_capacity(values.len());
    for (value, primary_dot) in values {
        match co_values.remove(&value) {
            Some(actors) if !actors.is_empty() => {
                if !primary_dot.is_attributed() {
                    attributions.push((value.clone(), primary_dot));
                }
                attributions.extend(actors.into_values().map(|dot| (value.clone(), dot)));
            }
            _ => attributions.push((value, primary_dot)),
        }
    }
    attributions.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
        left_dot
            .cmp(right_dot)
            .then_with(|| left_value.cmp(right_value))
    });
    let by_value = retain_fair_data_elements(attributions, max_len);
    let mut primary = by_value
        .into_iter()
        .filter_map(|(value, mut actor_dots)| {
            actor_dots.sort();
            actor_dots
                .pop()
                .map(|primary_dot| (value, primary_dot, actor_dots))
        })
        .collect::<Vec<_>>();
    primary.sort_by(|(left_value, left_dot, _), (right_value, right_dot, _)| {
        left_dot
            .cmp(right_dot)
            .then_with(|| left_value.cmp(right_value))
    });
    let mut data = Vec::with_capacity(primary.len());
    let mut dots = Vec::with_capacity(primary.len());
    let mut co_dots = Vec::new();
    for (index, (value, primary_dot, extra_dots)) in primary.into_iter().enumerate() {
        if let Ok(index_u32) = u32::try_from(index) {
            co_dots.extend(extra_dots.into_iter().map(|dot| (index_u32, dot)));
        }
        data.push(value);
        dots.push(primary_dot);
    }
    (data, dots, co_dots)
}

/// Retain up to `max_len` unique `Bytes` values from `visible` (already sorted ascending by
/// `(dot, value)`), bounding each authenticated writer (`!dot.version.actor.is_default()`) by
/// water-filling recency rank so neither single-writer floods nor multi-writer co-dot amplification
/// can evict honest writers' newest elements (#1179, #1220).
pub(super) fn retain_fair_data_elements(
    visible: Vec<(Bytes, EntryDot)>,
    max_len: usize,
) -> BTreeMap<Bytes, Vec<EntryDot>> {
    let total = visible.len();
    let (_, mut ranked) = visible.into_iter().rev().fold(
        (BTreeMap::<Did, usize>::new(), Vec::with_capacity(total)),
        |(mut seen_per_actor, mut ranked), (value, dot)| {
            let rank = dot.attributed_actor().map_or(usize::MAX, |actor| {
                let count = seen_per_actor.entry(actor).or_insert(0);
                std::mem::replace(count, count.saturating_add(1))
            });
            ranked.push((rank, value, dot));
            (seen_per_actor, ranked)
        },
    );
    ranked.sort_by(
        |(left_rank, left_value, left_dot), (right_rank, right_value, right_dot)| {
            left_rank
                .cmp(right_rank)
                .then_with(|| Reverse(*left_dot).cmp(&Reverse(*right_dot)))
                .then_with(|| Reverse(left_value).cmp(&Reverse(right_value)))
        },
    );
    let mut by_value = BTreeMap::<Bytes, Vec<EntryDot>>::new();
    for (_, value, dot) in ranked {
        if by_value.len() < max_len {
            by_value.entry(value).or_default().push(dot);
        } else if let Some(dots) = by_value.get_mut(&value) {
            dots.push(dot);
        }
    }
    by_value
}
