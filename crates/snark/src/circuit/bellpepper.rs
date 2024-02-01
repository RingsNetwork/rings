//! implement bellpepper proof system for circuit

use super::Circuit;
use crate::prelude::bellpepper;
use crate::prelude::bellpepper::num::AllocatedNum;
use crate::prelude::bellpepper::ConstraintSystem;
use crate::prelude::bellpepper::LinearCombination;
use crate::prelude::bellpepper::SynthesisError;
use crate::prelude::ff::PrimeField;

impl<F: PrimeField> bellpepper::Circuit<F> for Circuit<F> {
    /// Reference work is Nota-Scotia: <https://github.com/nalinbhardwaj/Nova-Scotia>
    fn synthesize<CS: ConstraintSystem<F>>(self, cs: &mut CS) -> Result<(), SynthesisError> {
        let mut vars: Vec<AllocatedNum<F>> = vec![];

        for i in 1..self.r1cs.num_inputs {
            let f: F = self.witness[i];
            let v = AllocatedNum::alloc(cs.namespace(|| format!("public_{}", i)), || Ok(f))?;

            vars.push(v);
        }

        for i in 0..self.r1cs.num_aux {
            // Private witness trace
            let f: F = self.witness[i + self.r1cs.num_inputs];
            let v = AllocatedNum::alloc(cs.namespace(|| format!("aux_{}", i)), || Ok(f))?;
            vars.push(v);
        }

        let make_lc = |lc_data: Vec<(usize, F)>| {
            let res = lc_data.iter().fold(
                LinearCombination::<F>::zero(),
                |lc: LinearCombination<F>, (index, coeff)| {
                    lc + if *index > 0 {
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

        Ok(())
    }
}
