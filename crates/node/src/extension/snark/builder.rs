#![warn(missing_docs)]
//! [`SNARKTaskBuilder`] — loads an r1cs + witness calculator (local or remote), generates
//! the recursive circuits, and assembles the per-curve [`SNARKProofTask`] (public params +
//! initial fold).

use rings_derive::wasm_export;
use rings_snark::circuit;
use rings_snark::prelude::nova::provider;
use rings_snark::prelude::nova::provider::hyperkzg;
use rings_snark::prelude::nova::provider::ipa_pc;
use rings_snark::prelude::nova::spartan;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::r1cs;
use rings_snark::snark::SNARK;

use super::Circuit;
use super::CircuitEnum;
use super::CircuitGenerator;
use super::FieldEnum;
use super::Input;
use super::SNARKGenerator;
use super::SupportedPrimeField;
use crate::error::Result;
use crate::extension::types::snark::SNARKProofTask;

/// Snark builder
#[wasm_export]
pub struct SNARKTaskBuilder {
    circuit_generator: CircuitGenerator,
}

#[wasm_export]
impl SNARKTaskBuilder {
    /// Load r1cs sand witness from local path
    pub async fn from_local(
        r1cs_path: String,
        witness_wasm_path: String,
        field: SupportedPrimeField,
    ) -> Result<SNARKTaskBuilder> {
        match field {
            SupportedPrimeField::Vesta => {
                type F = <provider::VestaEngine as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Local(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Local(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Vesta(circuit_generator),
                })
            }
            SupportedPrimeField::Pallas => {
                type F = <provider::PallasEngine as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Local(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Local(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Pallas(circuit_generator),
                })
            }
            SupportedPrimeField::Bn256KZG => {
                type F = <provider::Bn256EngineKZG as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Local(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Local(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Bn256KZG(circuit_generator),
                })
            }
        }
    }

    /// Load r1cs sand witness from remote url
    pub async fn from_remote(
        r1cs_path: String,
        witness_wasm_path: String,
        field: SupportedPrimeField,
    ) -> Result<SNARKTaskBuilder> {
        match field {
            SupportedPrimeField::Vesta => {
                type F = <provider::VestaEngine as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Remote(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Remote(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Vesta(circuit_generator),
                })
            }
            SupportedPrimeField::Pallas => {
                type F = <provider::PallasEngine as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Remote(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Remote(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Pallas(circuit_generator),
                })
            }
            SupportedPrimeField::Bn256KZG => {
                type F = <provider::Bn256EngineKZG as Engine>::Scalar;
                let r1cs =
                    r1cs::load_r1cs::<F>(r1cs::Path::Remote(r1cs_path), r1cs::Format::Bin).await?;
                let witness_calculator =
                    r1cs::load_circom_witness_calculator(r1cs::Path::Remote(witness_wasm_path))
                        .await?;
                let circuit_generator =
                    circuit::WasmCircuitGenerator::<F>::new(r1cs, witness_calculator);
                Ok(Self {
                    circuit_generator: CircuitGenerator::Bn256KZG(circuit_generator),
                })
            }
        }
    }

    /// generate recursive circuits
    pub fn gen_circuits(
        &self,
        public_input: Input,
        private_inputs: Vec<Input>,
        round: usize,
    ) -> Result<Vec<Circuit>> {
        match &self.circuit_generator {
            CircuitGenerator::Vesta(g) => {
                type F = <provider::VestaEngine as Engine>::Scalar;

                let input: circuit::Input<F> = public_input
                    .into_iter()
                    .map(|(s, v)| {
                        (
                            s,
                            v.into_iter()
                                .map(|inp| {
                                    if let FieldEnum::Vesta(x) = inp.value {
                                        x
                                    } else {
                                        panic!("Wrong curve, expect Vesta")
                                    }
                                })
                                .collect(),
                        )
                    })
                    .collect::<Vec<(String, Vec<F>)>>()
                    .into();

                let private_inputs: Vec<circuit::Input<F>> = private_inputs
                    .into_iter()
                    .map(|inp| {
                        inp.into_iter()
                            .map(|(s, v)| {
                                let fields: Vec<F> = v
                                    .into_iter()
                                    .map(|inp| {
                                        if let FieldEnum::Vesta(x) = inp.value {
                                            x
                                        } else {
                                            panic!("Wrong curve, expect Vesta")
                                        }
                                    })
                                    .collect();
                                (s, fields)
                            })
                            .collect::<Vec<(String, Vec<F>)>>()
                            .into()
                    })
                    .collect();

                let circuits = g
                    .gen_recursive_circuit(input, private_inputs, round, true)?
                    .iter()
                    .map(|c| Circuit {
                        inner: CircuitEnum::Vesta(c.clone()),
                    })
                    .collect::<Vec<Circuit>>();
                Ok(circuits)
            }
            CircuitGenerator::Pallas(g) => {
                type F = <provider::PallasEngine as Engine>::Scalar;

                let input: circuit::Input<F> = public_input
                    .into_iter()
                    .map(|(s, v)| {
                        (
                            s,
                            v.into_iter()
                                .map(|inp| {
                                    if let FieldEnum::Pallas(x) = inp.value {
                                        x
                                    } else {
                                        panic!("Wrong curve, expect pallas")
                                    }
                                })
                                .collect(),
                        )
                    })
                    .collect::<Vec<(String, Vec<F>)>>()
                    .into();

                let private_inputs: Vec<circuit::Input<F>> = private_inputs
                    .into_iter()
                    .map(|inp| {
                        inp.into_iter()
                            .map(|(s, v)| {
                                let fields: Vec<F> = v
                                    .into_iter()
                                    .map(|inp| {
                                        if let FieldEnum::Pallas(x) = inp.value {
                                            x
                                        } else {
                                            panic!("Wrong curve, expect Vesta")
                                        }
                                    })
                                    .collect();
                                (s, fields)
                            })
                            .collect::<Vec<(String, Vec<F>)>>()
                            .into()
                    })
                    .collect();

                let circuits = g
                    .gen_recursive_circuit(input, private_inputs, round, true)?
                    .iter()
                    .map(|c| Circuit {
                        inner: CircuitEnum::Pallas(c.clone()),
                    })
                    .collect::<Vec<Circuit>>();
                Ok(circuits)
            }
            CircuitGenerator::Bn256KZG(g) => {
                type F = <provider::Bn256EngineKZG as Engine>::Scalar;

                let input: circuit::Input<F> = public_input
                    .into_iter()
                    .map(|(s, v)| {
                        (
                            s,
                            v.into_iter()
                                .map(|inp| {
                                    if let FieldEnum::Bn256KZG(x) = inp.value {
                                        x
                                    } else {
                                        panic!("Wrong curve, expect bn256")
                                    }
                                })
                                .collect(),
                        )
                    })
                    .collect::<Vec<(String, Vec<F>)>>()
                    .into();

                let private_inputs: Vec<circuit::Input<F>> = private_inputs
                    .into_iter()
                    .map(|inp| {
                        inp.into_iter()
                            .map(|(s, v)| {
                                let fields: Vec<F> = v
                                    .into_iter()
                                    .map(|inp| {
                                        if let FieldEnum::Bn256KZG(x) = inp.value {
                                            x
                                        } else {
                                            panic!("Wrong curve, expect bn256")
                                        }
                                    })
                                    .collect();
                                (s, fields)
                            })
                            .collect::<Vec<(String, Vec<F>)>>()
                            .into()
                    })
                    .collect();

                let circuits = g
                    .gen_recursive_circuit(input, private_inputs, round, true)?
                    .iter()
                    .map(|c| Circuit {
                        inner: CircuitEnum::Bn256KZG(c.clone()),
                    })
                    .collect::<Vec<Circuit>>();
                Ok(circuits)
            }
        }
    }
}

impl SNARKTaskBuilder {
    /// Generate proof task
    pub fn gen_proof_task(circuits: Vec<Circuit>) -> Result<SNARKProofTask> {
        let task = match &circuits[0].inner {
            CircuitEnum::Vesta(_) => {
                type E1 = provider::VestaEngine;
                type E2 = provider::PallasEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let circuits: Vec<circuit::Circuit<<E1 as Engine>::Scalar>> = circuits
                    .into_iter()
                    .map(|circ| {
                        if let CircuitEnum::Vesta(c) = circ.inner {
                            c
                        } else {
                            panic!("Wrong curve, expect vesta")
                        }
                    })
                    .collect();
                let inputs = circuits[0].get_public_inputs();
                let pp = SNARK::<E1, E2>::gen_pp::<S1, S2>(circuits[0].clone())?;
                let snark = SNARK::<E1, E2>::new(&circuits[0], &pp, &inputs, &vec![
                    <E2 as Engine>::Scalar::from(0),
                ])?;

                SNARKProofTask::VastaPallas(SNARKGenerator {
                    pp: pp.into(),
                    snark,
                    circuits,
                })
            }
            CircuitEnum::Pallas(_) => {
                type E1 = provider::PallasEngine;
                type E2 = provider::VestaEngine;
                type EE1 = ipa_pc::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>;
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>;
                let circuits: Vec<circuit::Circuit<<E1 as Engine>::Scalar>> = circuits
                    .into_iter()
                    .map(|circ| {
                        if let CircuitEnum::Pallas(c) = circ.inner {
                            c
                        } else {
                            panic!("Wrong curve, expect vesta")
                        }
                    })
                    .collect();
                let inputs = circuits[0].get_public_inputs();
                let pp = SNARK::<E1, E2>::gen_pp::<S1, S2>(circuits[0].clone())?;
                let snark = SNARK::<E1, E2>::new(&circuits[0], &pp, &inputs, &vec![
                    <E2 as Engine>::Scalar::from(0),
                ])?;
                SNARKProofTask::PallasVasta(SNARKGenerator {
                    pp: pp.into(),
                    snark,
                    circuits,
                })
            }
            CircuitEnum::Bn256KZG(_) => {
                type E1 = provider::Bn256EngineKZG;
                type E2 = provider::GrumpkinEngine;
                type EE1 = hyperkzg::EvaluationEngine<E1>;
                type EE2 = ipa_pc::EvaluationEngine<E2>;
                type S1 = spartan::snark::RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
                type S2 = spartan::snark::RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
                let circuits: Vec<circuit::Circuit<<E1 as Engine>::Scalar>> = circuits
                    .into_iter()
                    .map(|circ| {
                        if let CircuitEnum::Bn256KZG(c) = circ.inner {
                            c
                        } else {
                            panic!("Wrong curve, expect vesta")
                        }
                    })
                    .collect();
                let inputs = circuits[0].get_public_inputs();
                let pp = SNARK::<E1, E2>::gen_pp::<S1, S2>(circuits[0].clone())?;
                let snark = SNARK::<E1, E2>::new(&circuits[0], &pp, &inputs, &vec![
                    <E2 as Engine>::Scalar::from(0),
                ])?;
                SNARKProofTask::Bn256KZGGrumpkin(SNARKGenerator {
                    pp: pp.into(),
                    snark,
                    circuits,
                })
            }
        };
        Ok(task)
    }
}
