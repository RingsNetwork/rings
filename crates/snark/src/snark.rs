//! Implementation of Rings Snark
//! ==============
use nova_snark::traits::circuit::TrivialCircuit;
use nova_snark::traits::evaluation::EvaluationEngineTrait;
use nova_snark::traits::snark::RelaxedR1CSSNARKTrait;
use nova_snark::traits::Engine;
use nova_snark::PublicParams;

use crate::circuit::Circuit;

/// Recursive SNARK implementation
pub struct RecursiveSNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// primary circuit
    pub circuit_primary: Circuit<<E1 as Engine>::Scalar>,
    /// secondary circuit
    pub circuit_secondary: TrivialCircuit<E2::Scalar>,
    /// public params
    pub pp: PublicParams<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
    >,
}

impl<E1, E2> RecursiveSNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    /// Create public params with circom
    pub fn new<EE1, EE2, S1, S2>(circom: Circuit<E1::Scalar>) -> Self
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
        Self {
            circuit_primary,
            circuit_secondary,
            pp,
        }
    }
}
