//! Implementation of Rings Snark
//! ==============
#![allow(clippy::type_complexity)]
mod impls;
use std::ops::Deref;
use std::ops::DerefMut;

use serde::Deserialize;
use serde::Serialize;

use crate::circuit::Circuit;
use crate::circuit::Input;
use crate::error::Result;
use crate::prelude::nova;
use crate::prelude::nova::traits::circuit::TrivialCircuit;
use crate::prelude::nova::traits::snark::RelaxedR1CSSNARKTrait;
use crate::prelude::nova::traits::Engine;
use crate::prelude::nova::CompressedSNARK;
use crate::prelude::nova::RecursiveSNARK;

/// Rings Snark implementation, a wrapper of nova's recursion snark and compressed snark
#[derive(Serialize, Deserialize)]
pub struct SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// recursive snark
    #[serde(flatten)]
    pub inner: RecursiveSNARK<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>,
}

/// Wrap of nova's public params
#[derive(Serialize, Deserialize)]
pub struct PublicParams<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// public params
    #[serde(flatten)]
    pub inner:
        nova::PublicParams<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>,
}

/// Wrap of nova's prover key
pub struct ProverKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    /// prove key
    pub pk: nova::ProverKey<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
        S1,
        S2,
    >,
}

/// Wrap of nova's verifier key
pub struct VerifierKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    /// verifier key
    pub vk: nova::VerifierKey<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
        S1,
        S2,
    >,
}

impl<E1, E2> SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// Create public params
    pub fn gen_pp<S1, S2>(circom: Circuit<E1::Scalar>) -> PublicParams<E1, E2>
    where
        S1: RelaxedR1CSSNARKTrait<E1>,
        S2: RelaxedR1CSSNARKTrait<E2>,
    {
        let circuit_primary = circom.clone();
        let circuit_secondary = TrivialCircuit::<E2::Scalar>::default();
        nova::PublicParams::setup(
            &circuit_primary,
            &circuit_secondary,
            S1::ck_floor().deref(),
            S2::ck_floor().deref(),
        )
        .into()
    }

    /// Create public params with circom, and public input
    pub fn new(
        circom: impl AsRef<Circuit<E1::Scalar>>,
        pp: impl AsRef<PublicParams<E1, E2>>,
        public_inputs: impl AsRef<Input<E1::Scalar>>,
        secondary_inputs: impl AsRef<[E2::Scalar]>,
    ) -> Result<Self> {
        // flat public input here
        let public_inputs = public_inputs.as_ref().flat();
        let circuit_secondary = TrivialCircuit::<E2::Scalar>::default();
        // default input for secondary on initialize round is [0]
        let inner = RecursiveSNARK::new(
            pp.as_ref(),
            circom.as_ref(),
            &circuit_secondary,
            public_inputs.as_ref(),
            secondary_inputs.as_ref(),
        )?;
        Ok(Self { inner })
    }

    /// This folder will create a new snark instance
    pub fn fold_clone(
        &self,
        pp: impl AsRef<PublicParams<E1, E2>>,
        circom: impl AsRef<Circuit<E1::Scalar>>,
    ) -> Result<Self> {
        // Create a new instance here
        let mut snark = Self {
            inner: self.deref().clone(),
        };
        snark.foldr(pp, circom)?;
        Ok(snark)
    }

    /// Fold next circuit
    pub fn foldr(
        &mut self,
        pp: impl AsRef<PublicParams<E1, E2>>,
        circom: impl AsRef<Circuit<E1::Scalar>>,
    ) -> Result<()> {
        let circuit_secondary = TrivialCircuit::<E2::Scalar>::default();
        let snark = self.deref_mut();
        snark.prove_step(pp.as_ref(), circom.as_ref(), &circuit_secondary)?;
        Ok(())
    }

    /// Verify the correctness of the `RecursiveSNARK`
    /// Gen compress snark
    pub fn verify(
        &self,
        pp: impl AsRef<PublicParams<E1, E2>>,
        num_steps: usize,
        z0_primary: impl AsRef<[E1::Scalar]>,
        z0_secondary: impl AsRef<[E2::Scalar]>,
    ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>)> {
        Ok(self.deref().verify(
            pp.as_ref(),
            num_steps,
            z0_primary.as_ref(),
            z0_secondary.as_ref(),
        )?)
    }

    /// Gen compress snark
    pub fn compress_setup<S1, S2>(
        pp: impl AsRef<PublicParams<E1, E2>>,
    ) -> Result<(ProverKey<E1, E2, S1, S2>, VerifierKey<E1, E2, S1, S2>)>
    where
        S1: RelaxedR1CSSNARKTrait<E1>,
        S2: RelaxedR1CSSNARKTrait<E2>,
    {
        let (pk, vk) = CompressedSNARK::setup(pp.as_ref())?;
        Ok((ProverKey { pk }, VerifierKey { vk }))
    }

    /// gen compress_proof
    pub fn compress_prove<S1, S2>(
        &self,
        pp: impl AsRef<PublicParams<E1, E2>>,
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
    >
    where
        S1: RelaxedR1CSSNARKTrait<E1>,
        S2: RelaxedR1CSSNARKTrait<E2>,
    {
        Ok(CompressedSNARK::<
            E1,
            E2,
            Circuit<<E1 as Engine>::Scalar>,
            TrivialCircuit<E2::Scalar>,
            S1,
            S2,
        >::prove(pp.as_ref(), pk.as_ref(), self)?)
    }

    /// gen compress_proof
    pub fn compress_verify<S1, S2>(
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
        num_steps: usize,
        public_inputs: impl AsRef<[E1::Scalar]>,
    ) -> Result<(Vec<E1::Scalar>, Vec<E2::Scalar>)>
    where
        S1: RelaxedR1CSSNARKTrait<E1>,
        S2: RelaxedR1CSSNARKTrait<E2>,
    {
        let z1 = vec![E2::Scalar::from(0)];
        Ok(proof
            .as_ref()
            .verify(vk.as_ref(), num_steps, public_inputs.as_ref(), &z1)?)
    }
}
