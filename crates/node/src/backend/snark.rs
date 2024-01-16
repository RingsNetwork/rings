use rings_snark::snark::SNARK;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::circuit::Circuit;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
pub struct SNARKTask<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    snark: SNARK<E1, E2>,
    circuits: Circuit<<E1 as Engine>::Scalar>
}
