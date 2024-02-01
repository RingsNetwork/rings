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
            cs.alloc_input(|| format!("variable {}", i), || Ok(self.witness[i]))?;
        }
        for i in 0..self.r1cs.num_aux {
            let f = self.witness[i + self.r1cs.num_inputs];
            cs.alloc(|| format!("aux {}", i), || Ok(f))?;
        }

        let make_index = |index| {
            if index < self.r1cs.num_inputs {
                Index::Input(index)
            } else {
                Index::Aux(index - self.r1cs.num_inputs)
            }
        };
        let make_lc = |lc_data: Vec<(usize, E::Fr)>| {
            lc_data.iter().fold(
                LinearCombination::<E>::zero(),
                |lc: LinearCombination<E>, (index, coeff)| {
                    lc + (*coeff, Variable::new_unchecked(make_index(*index)))
                },
            )
        };
        for (i, constraint) in self.r1cs.constraints.iter().enumerate() {
            // 0 * LC = 0 must be ignored
            if !((constraint.0.is_empty() || constraint.1.is_empty()) && constraint.2.is_empty()) {
                cs.enforce(
                    || format!("{}", i),
                    |_| make_lc(constraint.0.clone()),
                    |_| make_lc(constraint.1.clone()),
                    |_| make_lc(constraint.2.clone()),
                );
            }
        }
        Ok(())
    }
}
