//! SNARK Backend
//! ================

use rings_snark::circuit::Circuit;
use rings_snark::prelude::nova::traits::circuit::TrivialCircuit;
use rings_snark::prelude::nova::traits::snark::RelaxedR1CSSNARKTrait;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::prelude::nova::CompressedSNARK;
use rings_snark::snark::ProverKey;
use rings_snark::snark::PublicParams;
use rings_snark::snark::VerifierKey;
use rings_snark::snark::SNARK;
use serde::Deserialize;
use serde::Serialize;
use super::types::SNARKProof;
use rings_snark::prelude::nova::provider;
use crate::backend::types::MessageHandler;
use rings_snark::prelude::nova::provider::ipa_pc;
use rings_snark::prelude::nova::provider::mlkzg;
use rings_snark::prelude::nova::spartan;
use std::sync::Arc;
use crate::provider::Provider;
use rings_core::message::MessagePayload;
use crate::error::Result;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SNARKTaskBehaviour {
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SNARKTask<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    snark: SNARK<E1, E2>,
    circuits: Vec<Circuit<<E1 as Engine>::Scalar>>,
    pp: PublicParams<E1, E2>,
}

impl<E1, E2> SNARKTask<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// Setup snark, get pk and vk
    pub fn fold(&mut self) -> Result<()> {
        Ok(self.snark.fold_all(&self.pp, &self.circuits)?)
    }

    /// setup compressed snark, get (pk, vk)
    pub fn setup<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
    ) -> Result<(ProverKey<E1, E2, S1, S2>, VerifierKey<E1, E2, S1, S2>)> {
        Ok(SNARK::<E1, E2>::compress_setup(&self.pp)?)
    }

    /// gen proof for compressed snark
    pub fn prove<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
        pk: impl AsRef<ProverKey<E1, E2, S1, S2>>,
    ) -> Result<
        CompressedSNARK<
            E1,
            E2,
            Circuit<<E1 as Engine>::Scalar>,
            TrivialCircuit<E2::Scalar>,
            S1,
            S2,
        >,
    > {
        Ok(self.snark.compress_prove(&self.pp, pk)?)
    }

    /// verify a proof
    pub fn verify<S1: RelaxedR1CSSNARKTrait<E1>, S2: RelaxedR1CSSNARKTrait<E2>>(
        &self,
        proof: impl AsRef<
            CompressedSNARK<
                E1,
                E2,
                Circuit<<E1 as Engine>::Scalar>,
                TrivialCircuit<E2::Scalar>,
                S1,
                S2,
            >,
        >,
        vk: impl AsRef<VerifierKey<E1, E2, S1, S2>>,
    ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>)> {
        let steps = self.circuits.len();
        let first_input = self.circuits.first().unwrap().get_public_inputs();
        Ok(SNARK::<E1, E2>::compress_verify(
            proof,
            vk,
            steps,
            &first_input,
        )?)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl MessageHandler<SNARKProof> for SNARKTaskBehaviour {
     async fn handle_message(
        &self,
        provider: Arc<Provider>,
        ctx: &MessagePayload,
        data: &SNARKProof,
     ) -> std::result::Result<(), Box<dyn std::error::Error>> {
	 match data {
	     SNARKProof::VastaPallas(s) => {
		 type E1 = provider::VestaEngine;
		 type E2 = provider::PallasEngine;
		 type EE1 = ipa_pc::EvaluationEngine<E1>;
		 type EE2 = ipa_pc::EvaluationEngine<E2>;
		 type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
		 type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
		 let (pk, _vk) = s.setup()?;
		 let proof = s.prove::<S1, S2>(&pk)?;
		 Ok(())
	     },
	     SNARKProof::PallasVasta(s) => {
		 type E1 = provider::PallasEngine;
		 type E2 = provider::VestaEngine;
		 type EE1 = ipa_pc::EvaluationEngine<E1>;
		 type EE2 = ipa_pc::EvaluationEngine<E2>;
		 type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
		 type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
		 let (pk, _vk) = s.setup()?;
		 let proof = s.prove::<S1, S2>(&pk)?;
		 Ok(())
	     },
	     SNARKProof::Bn256KZGGrumpkin(s) => {
		 type E1 = provider::mlkzg::Bn256EngineKZG;
		 type E2 = provider::GrumpkinEngine;
		 type EE1 = mlkzg::EvaluationEngine<E1>;
		 type EE2 = ipa_pc::EvaluationEngine<E2>;
		 type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
		 type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
		 let (pk, _vk) = s.setup()?;
		 let proof = s.prove::<S1, S2>(&pk)?;
		 Ok(())
	     }
	 }
     }
}
