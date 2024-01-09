//! Implementation of Rings Snark
//! ==============
use ff::Field;
use nova_snark::traits::circuit::TrivialCircuit;
use nova_snark::traits::evaluation::EvaluationEngineTrait;
use nova_snark::traits::snark::RelaxedR1CSSNARKTrait;
use nova_snark::traits::Engine;
use nova_snark::PublicParams;
use nova_snark::RecursiveSNARK;

use crate::circuit::Circuit;
use crate::error::Result;

/// Recursive SNARK implementation
pub struct SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// recursive snark
    pub snark: RecursiveSNARK<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>,
}

impl<E1, E2> SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// Create public params with circom, and public input
    pub fn new<EE1, EE2, S1, S2>(
        circom: Circuit<E1::Scalar>,
        public_inputs: &[E1::Scalar],
    ) -> Result<Self>
    where
        EE1: EvaluationEngineTrait<E1>,
        EE2: EvaluationEngineTrait<E2>,
        S1: RelaxedR1CSSNARKTrait<E1>,
        S2: RelaxedR1CSSNARKTrait<E2>,
    {
        let circuit_primary = circom.clone();
        let circuit_secondary = TrivialCircuit::<E2::Scalar>::default();
        // Create pp here
        let pp = PublicParams::setup(
            &circuit_primary,
            &circuit_secondary,
            &*S1::ck_floor(),
            &*S2::ck_floor(),
        );
        // default input for secondary on initialize round is [0]
        let secondary_inputs = [<<E2 as Engine>::Scalar as Field>::ZERO];
        let snark = RecursiveSNARK::new(
            &pp,
            &circuit_primary,
            &circuit_secondary,
            public_inputs,
            &secondary_inputs,
        )?;

        Ok(Self { snark })
    }
}
