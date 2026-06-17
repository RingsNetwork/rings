//! SNARK Backend
//! ================

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use rings_core::dht::Did;
use rings_derive::wasm_export;
use rings_snark::circuit;
use rings_snark::prelude::nova::provider;
use rings_snark::prelude::nova::provider::hyperkzg;
use rings_snark::prelude::nova::provider::ipa_pc;
use rings_snark::prelude::nova::spartan;
use rings_snark::prelude::nova::traits::snark::RelaxedR1CSSNARKTrait;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::snark::CompressedSNARK;
use rings_snark::snark::ProverKey;
use rings_snark::snark::PublicParams;
use rings_snark::snark::VerifierKey;
use rings_snark::snark::SNARK;
use serde::Deserialize;
use serde::Serialize;

use super::types::snark::SNARKProofTask;
use super::types::snark::SNARKTask;
use super::types::snark::SNARKTaskMessage;
use super::types::snark::SNARKVerifyTask;
use crate::error::Error;
use crate::error::Result;
use crate::provider::Provider;

type TaskId = uuid::Uuid;

/// Namespace under which SNARK proof/verify tasks travel.
pub const NAMESPACE: &str = "snark";

#[cfg(feature = "browser")]
pub mod browser;
mod builder;
mod protocol;

pub use builder::SNARKTaskBuilder;
pub use protocol::SnarkProtocol;

/// Task Manageer of SNARK provider and verifier
#[derive(Default, Clone)]
pub struct SNARKTaskManager {
    /// map of task_id and task
    task: DashMap<TaskId, SNARKProofTask>,
    /// map of task_id and result
    verified: DashMap<TaskId, bool>,
}

/// SNARK message handler
#[wasm_export]
#[derive(Default, Clone)]
pub struct SNARKBehaviour {
    inner: Arc<SNARKTaskManager>,
}

impl std::ops::Deref for SNARKBehaviour {
    type Target = Arc<SNARKTaskManager>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl AsRef<SNARKProofTask> for &SNARKProofTask {
    fn as_ref(&self) -> &SNARKProofTask {
        self
    }
}

impl AsRef<SNARKVerifyTask> for &SNARKVerifyTask {
    fn as_ref(&self) -> &SNARKVerifyTask {
        self
    }
}

impl SNARKBehaviour {
    /// Generate proof task
    pub fn gen_proof_task(circuits: Vec<Circuit>) -> Result<SNARKProofTask> {
        SNARKTaskBuilder::gen_proof_task(circuits)
    }

    /// Generate a proof task and send it to did
    pub async fn gen_and_send_proof_task(
        &self,
        provider: Arc<Provider>,
        circuits: Vec<Circuit>,
        did: Did,
    ) -> Result<String> {
        let task = Self::gen_proof_task(circuits)?;
        self.send_proof_task(provider.clone(), &task, did).await
    }

    /// send proof task to did
    ///
    /// Sends a [`SNARKTaskMessage`] under the [`NAMESPACE`] envelope and records the
    /// task locally so the matching verify reply can be checked. Same code path on
    /// native and browser ([`Provider::send`]).
    pub async fn send_proof_task(
        &self,
        provider: Arc<Provider>,
        task_ref: impl AsRef<SNARKProofTask>,
        did: Did,
    ) -> Result<String> {
        let task_id = uuid::Uuid::new_v4();
        let task = task_ref.as_ref();
        let msg = SNARKTaskMessage {
            task_id,
            task: SNARKTask::SNARKProof(Box::new(task.clone())),
        };
        let payload = bincode::serialize(&msg).map_err(|_| Error::EncodeError)?;
        // Record the task *before* sending, so a fast verify reply cannot arrive before
        // the verifier has the proof task to check against. Roll back if the send fails.
        self.task.insert(task_id, task.clone());
        if let Err(e) = provider.send(did, NAMESPACE, Bytes::from(payload)).await {
            self.task.remove(&task_id);
            return Err(e);
        }
        tracing::info!("sent proof request");
        Ok(task_id.to_string())
    }

    /// Register the SNARK extension on a provider: the pure [`SnarkProtocol`] router paired
    /// with its `SnarkShell` interpreter (which owns this behaviour's task store and runs
    /// the proving/verification crypto). After this, inbound `snark` envelopes are
    /// dispatched automatically; results are readable via
    /// [`SNARKBehaviour::get_task_result`].
    pub fn register(&self, provider: &Provider) -> Result<()> {
        provider.register_protocol(SnarkProtocol, protocol::SnarkShell::new(self.inner.clone()))
    }
}

/// The state of a submitted proof task, from the requester's side.
///
/// A bare `bool` collapses two distinct outcomes into `false` — "no result yet" and "a
/// proof came back but failed verification" — so a pending task is indistinguishable from
/// an invalid one. This makes the distinction explicit.
#[wasm_export]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProofResult {
    /// No result has returned yet (still proving / in flight).
    Pending,
    /// A proof returned and **verified**.
    Verified,
    /// A proof returned but **failed verification**.
    Invalid,
}

#[wasm_export]
impl SNARKBehaviour {
    /// Current state of a submitted proof task: [`ProofResult::Pending`] until a result
    /// returns, then [`ProofResult::Verified`] / [`ProofResult::Invalid`]. Unlike a bare
    /// bool, this separates "not yet" from "verification failed".
    pub fn get_task_result(&self, task_id: String) -> Result<ProofResult> {
        let task_id = uuid::Uuid::parse_str(&task_id)?;
        Ok(match self.inner.verified.get(&task_id) {
            Some(v) if *v.value() => ProofResult::Verified,
            Some(_) => ProofResult::Invalid,
            None => ProofResult::Pending,
        })
    }
}

/// Types for circuit
pub enum CircuitGenerator {
    /// Circuit based on Vesta curve
    Vesta(circuit::WasmCircuitGenerator<<provider::VestaEngine as Engine>::Scalar>),
    /// Circuit based on pallas curve
    Pallas(circuit::WasmCircuitGenerator<<provider::PallasEngine as Engine>::Scalar>),
    /// Circuit based on KZG bn256
    Bn256KZG(circuit::WasmCircuitGenerator<<provider::Bn256EngineKZG as Engine>::Scalar>),
}

/// Supported prime field
#[wasm_export]
#[derive(Clone)]
pub enum SupportedPrimeField {
    /// field of vesta curve
    Vesta,
    /// field of pallas curve
    Pallas,
    /// bn256 with kzg
    Bn256KZG,
}

/// Input type
#[wasm_export]
#[derive(Deserialize, Serialize)]
pub struct Input(Vec<(String, Vec<Field>)>);

impl From<Vec<(String, Vec<Field>)>> for Input {
    fn from(data: Vec<(String, Vec<Field>)>) -> Self {
        Self(data)
    }
}

#[wasm_export]
impl Input {
    /// serialize Input to json
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// deserialize Input from json
    pub fn from_json(s: String) -> Result<Input> {
        Ok(serde_json::from_str(&s)?)
    }
}

impl std::ops::Deref for Input {
    type Target = Vec<(String, Vec<Field>)>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoIterator for Input {
    type Item = (String, Vec<Field>);
    type IntoIter = <Vec<Self::Item> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Input {
    type Item = <&'a Vec<(String, Vec<Field>)> as IntoIterator>::Item;
    type IntoIter = <&'a Vec<(String, Vec<Field>)> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Circuit, it's a typeless wrapper of rings_snark circuit
#[wasm_export]
#[derive(Deserialize, Serialize)]
pub struct Circuit {
    inner: CircuitEnum,
}

/// Types of Circuit
#[derive(Deserialize, Serialize)]
pub enum CircuitEnum {
    /// Based on vesta curve
    Vesta(circuit::Circuit<<provider::VestaEngine as Engine>::Scalar>),
    /// Based on pallas curve
    Pallas(circuit::Circuit<<provider::PallasEngine as Engine>::Scalar>),
    /// based on bn256 and KZG
    Bn256KZG(circuit::Circuit<<provider::Bn256EngineKZG as Engine>::Scalar>),
}

#[wasm_export]
impl Circuit {
    /// serialize circuit to json
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// deserialize circuit from json
    pub fn from_json(s: String) -> Result<Circuit> {
        Ok(serde_json::from_str(&s)?)
    }
}

/// Field type
#[wasm_export]
#[derive(Deserialize, Serialize)]
pub struct Field {
    value: FieldEnum,
}

/// Supported prime field
#[derive(Deserialize, Serialize)]
pub enum FieldEnum {
    /// field of vesta curve
    Vesta(<provider::VestaEngine as Engine>::Scalar),
    /// field of pallas curve
    Pallas(<provider::PallasEngine as Engine>::Scalar),
    /// bn256 with kzg
    Bn256KZG(<provider::Bn256EngineKZG as Engine>::Scalar),
}

#[wasm_export]
impl Field {
    /// create field from u64
    pub fn from_u64(v: u64, ty: SupportedPrimeField) -> Self {
        match ty {
            SupportedPrimeField::Vesta => Self {
                value: FieldEnum::Vesta(<provider::VestaEngine as Engine>::Scalar::from(v)),
            },
            SupportedPrimeField::Pallas => Self {
                value: FieldEnum::Pallas(<provider::PallasEngine as Engine>::Scalar::from(v)),
            },
            SupportedPrimeField::Bn256KZG => Self {
                value: FieldEnum::Bn256KZG(<provider::Bn256EngineKZG as Engine>::Scalar::from(v)),
            },
        }
    }
}

/// SNARK Proof
#[derive(Serialize, Deserialize)]
pub struct SNARKProof<E1, E2, S1, S2>
where
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// verifier key of proof
    #[serde(
        serialize_with = "crate::util::serialize_forward",
        deserialize_with = "crate::util::deserialize_forward"
    )]
    pub vk: VerifierKey<E1, E2, S1, S2>,
    #[serde(
        serialize_with = "crate::util::serialize_forward",
        deserialize_with = "crate::util::deserialize_forward"
    )]
    /// compressed proof
    pub proof: CompressedSNARK<E1, E2, S1, S2>,
}

/// SNARK proof generator, including setup, proof and verify
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SNARKGenerator<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    snark: SNARK<E1, E2>,
    circuits: Vec<circuit::Circuit<<E1 as Engine>::Scalar>>,
    pp: Arc<PublicParams<E1, E2>>,
}

impl SNARKProofTask {
    /// Make snark proof task splitable
    pub fn split(&self, n: usize) -> Vec<SNARKProofTask> {
        match self {
            SNARKProofTask::PallasVasta(g) => g
                .split(n)
                .into_iter()
                .map(SNARKProofTask::PallasVasta)
                .collect(),
            SNARKProofTask::VastaPallas(g) => g
                .split(n)
                .into_iter()
                .map(SNARKProofTask::VastaPallas)
                .collect(),
            SNARKProofTask::Bn256KZGGrumpkin(g) => g
                .split(n)
                .into_iter()
                .map(SNARKProofTask::Bn256KZGGrumpkin)
                .collect(),
        }
    }
}

impl<E1, E2> SNARKGenerator<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// Setup snark, get pk and vk, if check set to true, it will check the folding is working correct
    pub fn fold(&mut self, check: bool) -> Result<()> {
        self.snark.fold_all(&self.pp, &self.circuits)?;
        if check {
            let steps = self.circuits.len();
            let first_input = self.circuits.first().unwrap().get_public_inputs();
            self.snark
                .verify(&self.pp, steps, first_input, vec![E2::Scalar::from(0)])?;
        }
        Ok(())
    }

    /// Split a SNARKGenerator task to multiple, by split circuits into multiple
    pub fn split(&self, n: usize) -> Vec<Self> {
        let SNARKGenerator {
            snark,
            circuits,
            pp,
        } = self;

        let mut split = Vec::new();
        let chunk_size = circuits.len().div_ceil(n);

        for circuit_chunk in circuits.chunks(chunk_size) {
            let new_generator = SNARKGenerator {
                snark: snark.clone(),
                circuits: circuit_chunk.to_vec(),
                pp: Arc::clone(pp),
            };
            split.push(new_generator);
        }
        split
    }

    /// setup compressed snark, get (pk, vk)
    #[allow(clippy::type_complexity)]
    pub fn setup<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
    ) -> Result<(ProverKey<E1, E2, S1, S2>, VerifierKey<E1, E2, S1, S2>)> {
        Ok(SNARK::<E1, E2>::compress_setup(&self.pp)?)
    }

    /// gen proof for compressed snark
    pub fn prove<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
        pk: impl AsRef<ProverKey<E1, E2, S1, S2>>,
    ) -> Result<CompressedSNARK<E1, E2, S1, S2>> {
        Ok(self.snark.compress_prove(&self.pp, pk)?)
    }

    /// verify a proof
    #[allow(clippy::type_complexity)]
    pub fn verify<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
        proof: impl AsRef<CompressedSNARK<E1, E2, S1, S2>>,
        vk: impl AsRef<VerifierKey<E1, E2, S1, S2>>,
    ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>)> {
        let steps = self.circuits.len();
        let first_input = self.circuits.first().unwrap().get_public_inputs();
        Ok(SNARK::<E1, E2>::compress_verify(
            proof,
            vk,
            steps,
            first_input,
        )?)
    }
}

impl SNARKBehaviour {
    /// Handle proof task
    pub fn handle_snark_proof_task<T: AsRef<SNARKProofTask>>(data: T) -> Result<SNARKVerifyTask> {
        tracing::debug!("SNARK proof start");
        let ret = match data.as_ref() {
            SNARKProofTask::VastaPallas(s) => {
                type E1 = provider::VestaEngine;
                type E2 = provider::PallasEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let mut snark = s.clone();
                snark.fold(false)?;
                let (pk, vk) = snark.setup()?;
                let compressed_proof = snark.prove::<S1, S2>(&pk)?;
                let proof = SNARKProof::<E1, E2, S1, S2> {
                    vk,
                    proof: compressed_proof,
                };
                Ok(SNARKVerifyTask::VastaPallas(serde_json::to_string(&proof)?))
            }
            SNARKProofTask::PallasVasta(s) => {
                type E1 = provider::PallasEngine;
                type E2 = provider::VestaEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let mut snark = s.clone();
                snark.fold(false)?;
                let (pk, vk) = snark.setup()?;
                let compressed_proof = snark.prove::<S1, S2>(&pk)?;
                let proof = SNARKProof::<E1, E2, S1, S2> {
                    vk,
                    proof: compressed_proof,
                };
                Ok(SNARKVerifyTask::PallasVasta(serde_json::to_string(&proof)?))
            }
            SNARKProofTask::Bn256KZGGrumpkin(s) => {
                type E1 = provider::Bn256EngineKZG;
                type E2 = provider::GrumpkinEngine;
                type EE1 = hyperkzg::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
                let mut snark = s.clone();
                snark.fold(false)?;
                let (pk, vk) = snark.setup()?;
                let compressed_proof = snark.prove::<S1, S2>(&pk)?;
                let proof = SNARKProof::<E1, E2, S1, S2> {
                    vk,
                    proof: compressed_proof,
                };
                Ok(SNARKVerifyTask::Bn256KZGGrumpkin(serde_json::to_string(
                    &proof,
                )?))
            }
        };
        tracing::debug!("SNARK proof success");
        ret
    }

    /// Handle verify task
    pub fn handle_snark_verify_task<T: AsRef<SNARKVerifyTask>, F: AsRef<SNARKProofTask>>(
        data: T,
        snark: F,
    ) -> Result<bool> {
        tracing::debug!("SNARK verify start");
        let snark = snark.as_ref();
        let ret = match data.as_ref() {
            SNARKVerifyTask::PallasVasta(p) => {
                type E1 = provider::PallasEngine;
                type E2 = provider::VestaEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let proof = serde_json::from_str::<SNARKProof<E1, E2, S1, S2>>(p)?;
                if let SNARKProofTask::PallasVasta(t) = snark {
                    let ret = t.verify::<S1, S2>(proof.proof, proof.vk);
                    Ok(ret.is_ok())
                } else {
                    Err(Error::SNARKCurveNotMatch())
                }
            }
            SNARKVerifyTask::VastaPallas(p) => {
                type E1 = provider::VestaEngine;
                type E2 = provider::PallasEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let proof = serde_json::from_str::<SNARKProof<E1, E2, S1, S2>>(p)?;
                if let SNARKProofTask::VastaPallas(t) = snark {
                    let ret = t.verify::<S1, S2>(proof.proof, proof.vk);
                    Ok(ret.is_ok())
                } else {
                    Err(Error::SNARKCurveNotMatch())
                }
            }
            SNARKVerifyTask::Bn256KZGGrumpkin(p) => {
                type E1 = provider::Bn256EngineKZG;
                type E2 = provider::GrumpkinEngine;
                type EE1 = hyperkzg::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
                let proof = serde_json::from_str::<SNARKProof<E1, E2, S1, S2>>(p)?;
                if let SNARKProofTask::Bn256KZGGrumpkin(t) = snark {
                    let ret = t.verify::<S1, S2>(proof.proof, proof.vk);
                    Ok(ret.is_ok())
                } else {
                    Err(Error::SNARKCurveNotMatch())
                }
            }
        };
        tracing::debug!("SNARK verify success");
        ret
    }
}

impl From<SNARKGenerator<provider::PallasEngine, provider::VestaEngine>> for SNARKProofTask {
    fn from(snark: SNARKGenerator<provider::PallasEngine, provider::VestaEngine>) -> Self {
        Self::PallasVasta(snark)
    }
}

impl From<SNARKGenerator<provider::VestaEngine, provider::PallasEngine>> for SNARKProofTask {
    fn from(snark: SNARKGenerator<provider::VestaEngine, provider::PallasEngine>) -> Self {
        Self::VastaPallas(snark)
    }
}

impl From<SNARKGenerator<provider::Bn256EngineKZG, provider::GrumpkinEngine>> for SNARKProofTask {
    fn from(snark: SNARKGenerator<provider::Bn256EngineKZG, provider::GrumpkinEngine>) -> Self {
        Self::Bn256KZGGrumpkin(snark)
    }
}
