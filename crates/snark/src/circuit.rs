//! Recursive SNARK implementation
use bellpepper_core::num::AllocatedNum;
use bellpepper_core::ConstraintSystem;
use bellpepper_core::LinearCombination;
use bellpepper_core::SynthesisError;
use circom_scotia::r1cs::R1CS;
use ff::PrimeField;
use nova_snark::traits::circuit::StepCircuit;
use crate::r1cs::TyWitness;

/// Input of witness
pub type TyInput<F> = (String, Vec<F>);
/// type of sanity check
pub type TySanityCheck = bool;
/// Type of witness calculator
pub type TyWitnessCalculator<F> = fn(Vec<TyInput<F>>, TySanityCheck) -> TyWitness<F>;

/// Circuit
#[derive(Clone, Debug)]
pub struct Circuit<F: PrimeField> {
    r1cs: R1CS<F>,
    witness: Option<TyWitness<F>>,
}

/// Reference work: Nota-Scotia :: CircomCircuit
/// https://github.com/nalinbhardwaj/Nova-Scotia/blob/main/src/circom/circuit.rs
/// NOTE: assumes exactly half of the (public inputs + outputs) are outputs
impl<F: PrimeField> Circuit<F> {
    pub fn get_public_outputs(&self) -> Vec<F> {
        let pub_output_count = (self.r1cs.num_inputs - 1) / 2;
        let mut z_out: Vec<F> = vec![];
        for i in 1..self.r1cs.num_inputs {
            // Public inputs do not exist, so we alloc, and later enforce equality from z values
            let f: F = {
                match &self.witness {
                    None => F::ONE,
                    Some(w) => w[i],
                }
            };

            if i <= pub_output_count {
                // public output
                z_out.push(f);
            }
        }
        z_out
    }

    /// Simple synthesize
    pub fn vanilla_synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
    ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
        let witness = &self.witness;

        let mut vars: Vec<AllocatedNum<F>> = vec![];
        let mut z_out: Vec<AllocatedNum<F>> = vec![];
        let pub_output_count = (self.r1cs.num_inputs - 1) / 2;

        for i in 1..self.r1cs.num_inputs {
            // Public inputs do not exist, so we alloc, and later enforce equality from z values
            let f: F = {
                match witness {
                    None => F::ONE,
                    Some(w) => w[i],
                }
            };
            let v = AllocatedNum::alloc(cs.namespace(|| format!("public_{}", i)), || Ok(f))?;

            vars.push(v.clone());
            if i <= pub_output_count {
                // public output
                z_out.push(v);
            }
        }
        for i in 0..self.r1cs.num_aux {
            // Private witness trace
            let f: F = {
                match witness {
                    None => F::ONE,
                    Some(w) => w[i + self.r1cs.num_inputs],
                }
            };

            let v = AllocatedNum::alloc(cs.namespace(|| format!("aux_{}", i)), || Ok(f))?;
            vars.push(v);
        }

        let make_lc = |lc_data: Vec<(usize, F)>| {
            let res = lc_data.iter().fold(
                LinearCombination::<F>::zero(),
                |lc: LinearCombination<F>, (index, coeff)| {
                    lc + if *index > 0 as usize {
                        (*coeff, vars[*index - 1].get_variable())
                    } else {
                        (*coeff, CS::one())
                    }
                },
            );
            res
        };
        for (i, constraint) in self.r1cs.constraints.iter().enumerate() {
            cs.enforce(
                || format!("constraint {}", i),
                |_| make_lc(constraint.0.clone()),
                |_| make_lc(constraint.1.clone()),
                |_| make_lc(constraint.2.clone()),
            );
        }

        for i in (pub_output_count + 1)..self.r1cs.num_inputs {
            cs.enforce(
                || format!("pub input enforce {}", i),
                |lc| lc + z[i - 1 - pub_output_count].get_variable(),
                |lc| lc + CS::one(),
                |lc| lc + vars[i - 1].get_variable(),
            );
        }

        Ok(z_out)
    }
}

/// Implement StepCircuit for our Circuit
/// Reference work: Nota-Scotia :: CircomCircuit
/// https://github.com/nalinbhardwaj/Nova-Scotia/blob/main/src/circom/circuit.rs
impl<F: PrimeField> StepCircuit<F> for Circuit<F> {
    fn arity(&self) -> usize {
        (self.r1cs.num_inputs - 1) / 2
    }

    fn synthesize<CS: ConstraintSystem<F>>(
        &self,
        cs: &mut CS,
        z: &[AllocatedNum<F>],
    ) -> Result<Vec<AllocatedNum<F>>, SynthesisError> {
        // synthesize the circuit
        let z_out = self.vanilla_synthesize(cs, z);

        z_out
    }
}
