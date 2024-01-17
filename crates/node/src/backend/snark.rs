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

use crate::error::Result;

#[derive(Serialize, Deserialize)]
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
