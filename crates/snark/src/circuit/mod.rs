//! Implementation of Circuit
//! ==========================
use std::cell::RefCell;
use std::iter::Iterator;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use bellpepper_core::num::AllocatedNum;
use bellpepper_core::ConstraintSystem;
use bellpepper_core::LinearCombination;
use bellpepper_core::SynthesisError;
use ff::PrimeField;
use nova_snark::traits::circuit::StepCircuit;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

use crate::error::Error;
use crate::error::Result;
use crate::r1cs::R1CS;
use crate::witness::calculator::WitnessCalculator;

pub mod bellman;
pub mod bellpepper;

/// Input of witness
#[derive(Serialize, Deserialize, Clone)]
pub struct Input<F: PrimeField> {
    /// inner input
    pub input: Vec<(String, Vec<F>)>,
}

impl<F: PrimeField> AsRef<Input<F>> for Input<F> {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<F: PrimeField> Deref for Input<F> {
    type Target = Vec<(String, Vec<F>)>;
    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

impl<F: PrimeField> DerefMut for Input<F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.input
    }
}

impl<F: PrimeField> Input<F> {
    /// flat input
    pub fn flat(&self) -> Vec<F> {
        self.input
            .clone()
            .into_iter()
            .flat_map(|(_, v)| v)
            .collect()
    }

    /// Get flat length of input
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.input
            .iter()
            .flat_map(|(_, v)| v)
            .collect::<Vec<&F>>()
            .len()
    }
}

impl<F: PrimeField> IntoIterator for Input<F> {
    type Item = (String, Vec<F>);
    type IntoIter = <Vec<Self::Item> as IntoIterator>::IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        self.input.into_iter()
    }
}

impl<'a, F: PrimeField> IntoIterator for &'a Input<F> {
    type Item = <&'a Vec<(String, Vec<F>)> as IntoIterator>::Item;
    type IntoIter = <&'a Vec<(String, Vec<F>)> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.input.iter()
    }
}

impl<F: PrimeField> From<Vec<(String, Vec<F>)>> for Input<F> {
    fn from(input: Vec<(String, Vec<F>)>) -> Self {
        Self { input }
    }
}

/// Circuit
#[derive(Serialize, Clone, Debug)]
pub struct Circuit<F: PrimeField> {
    r1cs: Arc<R1CS<F>>,
    witness: Vec<F>,
}

impl<'de, F> Deserialize<'de> for Circuit<F>
where F: PrimeField + Deserialize<'de>
{
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where D: Deserializer<'de> {
        #[derive(Deserialize)]
        #[serde(bound(deserialize = "F: Deserialize<'de>"))]
        struct CircuitData<F: PrimeField> {
            r1cs: Arc<R1CS<F>>,
            witness: Vec<F>,
        }

        let data = CircuitData::deserialize(deserializer)?;
        Self::try_new(data.r1cs, data.witness).map_err(serde::de::Error::custom)
    }
}

impl<F: PrimeField> AsRef<Circuit<F>> for &Circuit<F> {
    fn as_ref(&self) -> &Circuit<F> {
        self
    }
}

/// Wasm based circuit generator
pub struct WasmCircuitGenerator<F: PrimeField> {
    r1cs: Arc<R1CS<F>>,
    calculator: Rc<RefCell<WitnessCalculator>>,
}

impl<F: PrimeField> WasmCircuitGenerator<F> {
    /// Crate new instance
    pub fn new(r1cs: R1CS<F>, calculator: WitnessCalculator) -> Self {
        Self {
            r1cs: Arc::new(r1cs),
            calculator: Rc::new(RefCell::new(calculator)),
        }
    }

    /// Generate iterator circuit list
    /// Which iterate inputs and generate circuit
    pub fn gen_circuit(&self, input: Input<F>, sanity_check: bool) -> Result<Circuit<F>>
    where F: PrimeField {
        let mut calc = self.calculator.borrow_mut();
        let witness: Vec<F> = calc.calculate_witness::<F>(input.to_vec(), sanity_check)?;
        Circuit::<F>::try_new(self.r1cs.clone(), witness)
    }

    /// Generate recursive circuit list
    /// Which use $output_{i-1}$ as $input_i$
    pub fn gen_recursive_circuit(
        &self,
        public_input: Input<F>,
        private_inputs: Vec<Input<F>>,
        times: usize,
        sanity_check: bool,
    ) -> Result<Vec<Circuit<F>>>
    where
        F: PrimeField,
    {
        fn reshape<F: PrimeField>(input: &[(String, Vec<F>)], output: &[F]) -> Result<Input<F>> {
            let mut ret = vec![];
            let mut iter = output.iter();

            for (val, vec) in input.iter() {
                let size = vec.len();
                let mut new_vec: Vec<F> = Vec::with_capacity(size);
                for _ in 0..size {
                    let item = iter.next().ok_or_else(|| {
                        Error::InvalidDataWhenReadingR1CS(
                            "recursive public output is shorter than public input shape"
                                .to_string(),
                        )
                    })?;
                    new_vec.push(*item);
                }
                ret.push((val.clone(), new_vec));
            }
            Ok(ret.into())
        }

        let mut ret = vec![];
        let mut calc = self.calculator.borrow_mut();
        let mut latest_output: Input<F> = vec![].into();
        for i in 0..times {
            let witness: Vec<F> = if latest_output.is_empty() {
                let mut input = public_input.clone();
                if let Some(p) = private_inputs.get(i) {
                    input.input.extend(p.to_owned());
                }
                calc.calculate_witness::<F>(input.to_vec(), sanity_check)?
            } else {
                let mut input = latest_output.clone();
                if let Some(p) = private_inputs.get(i) {
                    input.input.extend(p.to_owned());
                }
                calc.calculate_witness::<F>(input.to_vec(), sanity_check)?
            };
            let circom = Circuit::<F>::try_new(self.r1cs.clone(), witness.clone())?;
            log::trace!("witness: {:?}, r1cs: {:?}", witness, self.r1cs);
            latest_output = reshape(&public_input, &circom.get_public_outputs()?)?;
            ret.push(circom);
        }
        Ok(ret)
    }
}

impl<F: PrimeField> Circuit<F> {
    /// Create a new instance after validating the R1CS and witness shape.
    pub fn try_new(r1cs: Arc<R1CS<F>>, witness: Vec<F>) -> Result<Self> {
        r1cs.validate_witness_len(witness.len())?;
        Ok(Self { r1cs, witness })
    }

    /// get public outputs from witness
    pub fn get_public_outputs(&self) -> Result<Vec<F>> {
        // witness: <1> <Outputs> <Inputs> <Auxs>
        // NOTE: assumes exactly half of the (public inputs + outputs) are outputs
        let range = self.public_output_range()?;
        self.witness
            .get(range.clone())
            .map(<[F]>::to_vec)
            .ok_or(Error::InvalidWitnessLength {
                expected: range.end,
                actual: self.witness.len(),
            })
    }

    /// get public inputs from witness
    pub fn get_public_inputs(&self) -> Result<Vec<F>> {
        // witness: <1> <Outputs> <Inputs> <Auxs>
        // NOTE: assumes exactly half of the (public inputs + outputs) are outputs
        let range = self.public_input_range()?;
        self.witness
            .get(range.clone())
            .map(<[F]>::to_vec)
            .ok_or(Error::InvalidWitnessLength {
                expected: range.end,
                actual: self.witness.len(),
            })
    }
}

/// Implement StepCircuit for our Circuit
/// Reference work: Nota-Scotia :: CircomCircuit
/// `<https://github.com/nalinbhardwaj/Nova-Scotia/blob/main/src/circom/circuit.rs>`
/// NOTE: assumes exactly half of the (public inputs + outputs) are outputs
impl<F: PrimeField> StepCircuit<F> for Circuit<F> {
    fn arity(&self) -> usize {
        (self.r1cs.num_inputs - 1) / 2
    }

    /// Simple synthesize
    fn synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
    ) -> core::result::Result<Vec<AllocatedNum<F>>, SynthesisError> {
        let mut vars: Vec<AllocatedNum<F>> = vec![];
        let mut z_out: Vec<AllocatedNum<F>> = vec![];
        let pub_output_count = self
            .public_output_count()
            .map_err(|_| SynthesisError::AssignmentMissing)?;

        for i in 1..self.r1cs.num_inputs {
            // Public inputs do not exist, so we alloc, and later enforce equality from z values
            let f = self.witness_value(i)?;
            let v = AllocatedNum::alloc(cs.namespace(|| format!("public_{i}")), || Ok(f))?;

            vars.push(v.clone());
            if i <= pub_output_count {
                // public output
                z_out.push(v);
            }
        }
        for i in 0..self.r1cs.num_aux {
            // Private witness trace
            let f = self.witness_value(i + self.r1cs.num_inputs)?;
            let v = AllocatedNum::alloc(cs.namespace(|| format!("aux_{i}")), || Ok(f))?;
            vars.push(v);
        }

        for (i, constraint) in self.r1cs.constraints.iter().enumerate() {
            let a = bellpepper_linear_combination::<CS, F>(&vars, &constraint.0)?;
            let b = bellpepper_linear_combination::<CS, F>(&vars, &constraint.1)?;
            let c = bellpepper_linear_combination::<CS, F>(&vars, &constraint.2)?;
            cs.enforce(
                || format!("constraint {i}"),
                |_| a.clone(),
                |_| b.clone(),
                |_| c.clone(),
            );
        }

        for i in (pub_output_count + 1)..self.r1cs.num_inputs {
            let Some(z_value) = z.get(i - 1 - pub_output_count) else {
                return Err(SynthesisError::AssignmentMissing);
            };
            let Some(var) = vars.get(i - 1) else {
                return Err(SynthesisError::AssignmentMissing);
            };
            let z_variable = z_value.get_variable();
            let public_variable = var.get_variable();
            cs.enforce(
                || format!("pub input enforce {i}"),
                |lc| lc + z_variable,
                |lc| lc + CS::one(),
                |lc| lc + public_variable,
            );
        }

        Ok(z_out)
    }
}

impl<F: PrimeField> Circuit<F> {
    fn witness_value(&self, index: usize) -> core::result::Result<F, SynthesisError> {
        self.witness
            .get(index)
            .copied()
            .ok_or(SynthesisError::AssignmentMissing)
    }

    fn public_output_count(&self) -> Result<usize> {
        self.r1cs.validate_witness_len(self.witness.len())?;
        self.r1cs.public_io_value_count()
    }

    fn public_output_range(&self) -> Result<Range<usize>> {
        let output_count = self.public_output_count()?;
        Ok(1..output_count + 1)
    }

    fn public_input_range(&self) -> Result<Range<usize>> {
        let output_count = self.public_output_count()?;
        Ok(1 + output_count..self.r1cs.num_inputs)
    }
}

pub(super) fn bellpepper_linear_combination<CS, F>(
    vars: &[AllocatedNum<F>],
    lc_data: &[(usize, F)],
) -> core::result::Result<LinearCombination<F>, SynthesisError>
where
    CS: ConstraintSystem<F>,
    F: PrimeField,
{
    let mut lc = LinearCombination::<F>::zero();
    for (index, coeff) in lc_data {
        let term = if *index == 0 {
            (*coeff, CS::one())
        } else {
            let variable_index = index
                .checked_sub(1)
                .ok_or(SynthesisError::AssignmentMissing)?;
            let var = vars
                .get(variable_index)
                .ok_or(SynthesisError::AssignmentMissing)?;
            (*coeff, var.get_variable())
        };
        lc = lc + term;
    }
    Ok(lc)
}

#[cfg(test)]
mod tests {
    use pasta_curves::Fp;

    use super::*;

    #[test]
    fn test_circuit_deserialization_rejects_invalid_r1cs_shape() {
        let value = serde_json::json!({
            "r1cs": {
                "num_inputs": 0,
                "num_aux": 0,
                "num_variables": 0,
                "constraints": []
            },
            "witness": []
        });

        let result = serde_json::from_value::<Circuit<Fp>>(value);
        assert!(result.is_err());
    }

    #[test]
    fn test_circuit_serialization_round_trip_preserves_validated_shape(
    ) -> std::result::Result<(), String> {
        let circuit = Circuit::try_new(
            Arc::new(R1CS {
                num_inputs: 3,
                num_aux: 1,
                num_variables: 4,
                constraints: Vec::new(),
            }),
            vec![Fp::from(1); 4],
        )
        .map_err(|error| error.to_string())?;
        let encoded = serde_json::to_string(&circuit).map_err(|error| error.to_string())?;
        let decoded =
            serde_json::from_str::<Circuit<Fp>>(&encoded).map_err(|error| error.to_string())?;
        let outputs = decoded
            .get_public_outputs()
            .map_err(|error| error.to_string())?;

        assert_eq!(outputs, vec![Fp::from(1)]);
        Ok(())
    }
}
