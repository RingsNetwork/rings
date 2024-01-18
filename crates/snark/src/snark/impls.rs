use std::ops::Deref;
use std::ops::DerefMut;

use super::CompressedSNARK;
use super::ProverKey;
use super::PublicParams;
use super::VerifierKey;
use super::SNARK;
use crate::circuit::Circuit;
use crate::prelude::nova;
use crate::prelude::nova::traits::circuit::TrivialCircuit;
use crate::prelude::nova::traits::snark::RelaxedR1CSSNARKTrait;
use crate::prelude::nova::traits::Engine;
use crate::prelude::nova::RecursiveSNARK;

impl<E1, E2> AsRef<SNARK<E1, E2>> for SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<E1, E2> Deref for SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    type Target =
        RecursiveSNARK<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<E1, E2> DerefMut for SNARK<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<E1, E2>
    From<nova::PublicParams<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>>
    for PublicParams<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    fn from(
        pp: nova::PublicParams<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>,
    ) -> Self {
        Self { inner: pp }
    }
}

impl<E1, E2> Deref for PublicParams<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    type Target =
        nova::PublicParams<E1, E2, Circuit<<E1 as Engine>::Scalar>, TrivialCircuit<E2::Scalar>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<E1, E2> DerefMut for PublicParams<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<E1, E2> AsRef<PublicParams<E1, E2>> for PublicParams<E1, E2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<E1, E2, S1, S2> Deref for ProverKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    type Target = nova::ProverKey<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
        S1,
        S2,
    >;
    fn deref(&self) -> &Self::Target {
        &self.pk
    }
}

impl<E1, E2, S1, S2> DerefMut for ProverKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pk
    }
}

impl<E1, E2, S1, S2> AsRef<ProverKey<E1, E2, S1, S2>> for ProverKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<E1, E2, S1, S2> Deref for VerifierKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    type Target = nova::VerifierKey<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
        S1,
        S2,
    >;
    fn deref(&self) -> &Self::Target {
        &self.vk
    }
}

impl<E1, E2, S1, S2> DerefMut for VerifierKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.vk
    }
}

impl<E1, E2, S1, S2> AsRef<VerifierKey<E1, E2, S1, S2>> for VerifierKey<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<E1, E2, S1, S2> Deref for CompressedSNARK<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    type Target = nova::CompressedSNARK<
        E1,
        E2,
        Circuit<<E1 as Engine>::Scalar>,
        TrivialCircuit<<E2 as Engine>::Scalar>,
        S1,
        S2,
    >;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<E1, E2, S1, S2> DerefMut for CompressedSNARK<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<E1, E2, S1, S2> AsRef<CompressedSNARK<E1, E2, S1, S2>> for CompressedSNARK<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<E1, E2, S1, S2>
    From<
        nova::CompressedSNARK<
            E1,
            E2,
            Circuit<<E1 as Engine>::Scalar>,
            TrivialCircuit<E2::Scalar>,
            S1,
            S2,
        >,
    > for CompressedSNARK<E1, E2, S1, S2>
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn from(
        snark: nova::CompressedSNARK<
            E1,
            E2,
            Circuit<<E1 as Engine>::Scalar>,
            TrivialCircuit<E2::Scalar>,
            S1,
            S2,
        >,
    ) -> Self {
        Self { inner: snark }
    }
}

impl<E1, E2, S1, S2>
    From<
        CompressedSNARK<E1, E2, S1, S2>,
    > for nova::CompressedSNARK<
            E1,
            E2,
            Circuit<<E1 as Engine>::Scalar>,
            TrivialCircuit<E2::Scalar>,
            S1,
            S2,
        >
where
    E1: Engine<Base = <E2 as Engine>::Scalar>,
    E2: Engine<Base = <E1 as Engine>::Scalar>,
    S1: RelaxedR1CSSNARKTrait<E1>,
    S2: RelaxedR1CSSNARKTrait<E2>,
{
    fn from(
        val: CompressedSNARK<E1, E2, S1, S2>,
    ) -> Self {
        val.inner
    }
}
