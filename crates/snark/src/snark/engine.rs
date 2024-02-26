//! Engines of Rings Snark
//! ============

use crate::prelude::nova::provider::Bn256Engine;
//use crate::prelude::nova::provider::GrumpkinEngine;
use nova_snark::traits::Engine as NovaEngine;
use crate::prelude::bellman::ScalarEngine;
use crate::prelude::bellman::pairing::bn256::Bn256;
use crate::prelude::nova::traits::commitment::CommitmentEngineTrait;
use crate::prelude::nova::traits::TranscriptEngineTrait;

/// A wrapper of Nova's Engine
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Engine<T: NovaEngine> {
    inner: T
}

impl <T: NovaEngine> AsRef<Engine<T>> for Engine<T> {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl <T: NovaEngine> AsRef<T> for Engine<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl ScalarEngine for Engine<Bn256Engine> {
    type Fr = <Bn256 as ScalarEngine>::Fr;
}

impl <T: NovaEngine> NovaEngine for Engine<T>
where
    <T as NovaEngine>::CE: CommitmentEngineTrait<Engine<T>>,
    <T as NovaEngine>::TE: TranscriptEngineTrait<Engine<T>>
{
    type Base = <T as NovaEngine>::Base;
    type Scalar = <T as NovaEngine>::Scalar;
    type GE = <T as NovaEngine>::GE;
    type RO = <T as NovaEngine>::RO;
    type ROCircuit = <T as NovaEngine>::ROCircuit;
    type TE = <T as NovaEngine>::TE;
    type CE = <T as NovaEngine>::CE;
}
