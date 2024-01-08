//! Implementation of Rings Snark
//! ==============
use circom_scotia::r1cs::R1CS;
use nova_snark::traits::circuit::TrivialCircuit;
use nova_snark::traits::evaluation::EvaluationEngineTrait;
use nova_snark::traits::snark::RelaxedR1CSSNARKTrait;
use nova_snark::traits::Engine;
use nova_snark::PublicParams;

use crate::circuit::Circuit;

/// Create public params with r1cs
pub fn create_public_params<E1, E2, EE1, EE2, S1, S2>(
    r1cs: R1CS<E1::Scalar>,
) -> PublicParams<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<<E2 as Engine>::Scalar>>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    EE1: EvaluationEngineTrait<E1>,
    EE2: EvaluationEngineTrait<E2>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    let circuit_primary = Circuit::<E1::Scalar>::new(r1cs, None);
    let circuit_secondary = TrivialCircuit::<E2::Scalar>::default();

    PublicParams::setup(
        &circuit_primary,
        &circuit_secondary,
        &*S1::ck_floor(),
        &*S2::ck_floor(),
    )
}
