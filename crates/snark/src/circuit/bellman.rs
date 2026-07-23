//! implement bellman proof system for circuit, this is useful for plonk and growth16

use super::Circuit;
use crate::prelude::bellman;
use crate::prelude::bellman::pairing::Engine;
use crate::prelude::bellman::ConstraintSystem;
use crate::prelude::bellman::Index;
use crate::prelude::bellman::LinearCombination;
use crate::prelude::bellman::SynthesisError;
use crate::prelude::bellman::Variable;

/// Previous work
/// <https://github.com/fluidex/plonkit/blob/master/src/circom_circuit.rs>
/// aux bias and input map are removed
impl<E: Engine> bellman::Circuit<E> for Circuit<E::Fr>
where E::Fr: ff::PrimeField
{
    //noinspection RsBorrowChecker
    fn synthesize<CS: ConstraintSystem<E>>(self, cs: &mut CS) -> Result<(), SynthesisError> {
        for i in 1..self.r1cs.num_inputs {
            let witness = self
                .witness
                .get(i)
                .copied()
                .ok_or(SynthesisError::AssignmentMissing)?;
            cs.alloc_input(|| format!("variable {i}"), || Ok(witness))?;
        }
        for i in 0..self.r1cs.num_aux {
            let f = self
                .witness
                .get(i + self.r1cs.num_inputs)
                .copied()
                .ok_or(SynthesisError::AssignmentMissing)?;
            cs.alloc(|| format!("aux {i}"), || Ok(f))?;
        }

        for (i, constraint) in self.r1cs.constraints.iter().enumerate() {
            // 0 * LC = 0 must be ignored
            if !((constraint.0.is_empty() || constraint.1.is_empty()) && constraint.2.is_empty()) {
                let a = bellman_linear_combination::<E>(&self.r1cs, &constraint.0)?;
                let b = bellman_linear_combination::<E>(&self.r1cs, &constraint.1)?;
                let c = bellman_linear_combination::<E>(&self.r1cs, &constraint.2)?;
                cs.enforce(
                    || format!("{i}"),
                    |_| a.clone(),
                    |_| b.clone(),
                    |_| c.clone(),
                );
            }
        }
        Ok(())
    }
}

fn bellman_linear_combination<E: Engine>(
    r1cs: &crate::r1cs::R1CS<E::Fr>,
    lc_data: &[(usize, E::Fr)],
) -> Result<LinearCombination<E>, SynthesisError>
where
    E::Fr: ff::PrimeField,
{
    let mut lc = LinearCombination::<E>::zero();
    for (index, coeff) in lc_data {
        let variable_index = checked_bellman_index(r1cs, *index)?;
        // Pre: checked_bellman_index proves that the wire index belongs to the
        // R1CS variable range. bellman exposes only new_unchecked for this type.
        let variable = Variable::new_unchecked(variable_index);
        lc = lc + (*coeff, variable);
    }
    Ok(lc)
}

fn checked_bellman_index<F: ff::PrimeField>(
    r1cs: &crate::r1cs::R1CS<F>,
    index: usize,
) -> Result<Index, SynthesisError> {
    if index >= r1cs.num_variables {
        return Err(SynthesisError::AssignmentMissing);
    }
    if index < r1cs.num_inputs {
        Ok(Index::Input(index))
    } else {
        Ok(Index::Aux(index - r1cs.num_inputs))
    }
}
