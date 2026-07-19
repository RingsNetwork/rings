//! implement bellpepper proof system for circuit

use super::bellpepper_linear_combination;
use super::Circuit;
use crate::prelude::bellpepper;
use crate::prelude::bellpepper::num::AllocatedNum;
use crate::prelude::bellpepper::ConstraintSystem;
use crate::prelude::bellpepper::SynthesisError;
use crate::prelude::ff::PrimeField;

impl<F: PrimeField> bellpepper::Circuit<F> for Circuit<F> {
    /// Reference work is Nota-Scotia: <https://github.com/nalinbhardwaj/Nova-Scotia>
    fn synthesize<CS: ConstraintSystem<F>>(self, cs: &mut CS) -> Result<(), SynthesisError> {
        let mut vars: Vec<AllocatedNum<F>> = vec![];

        for i in 1..self.r1cs.num_inputs {
            let f = self
                .witness
                .get(i)
                .copied()
                .ok_or(SynthesisError::AssignmentMissing)?;
            let v = AllocatedNum::alloc(cs.namespace(|| format!("public_{i}")), || Ok(f))?;

            vars.push(v);
        }

        for i in 0..self.r1cs.num_aux {
            // Private witness trace
            let f = self
                .witness
                .get(i + self.r1cs.num_inputs)
                .copied()
                .ok_or(SynthesisError::AssignmentMissing)?;
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

        Ok(())
    }
}
