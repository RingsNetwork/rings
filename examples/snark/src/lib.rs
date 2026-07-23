use std::error;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use rings_snark::circuit::Input;
use rings_snark::prelude::ff::PrimeField;
use rings_snark::prelude::nova::provider::ipa_pc::EvaluationEngine;
use rings_snark::prelude::nova::provider::PallasEngine;
use rings_snark::prelude::nova::provider::VestaEngine;
use rings_snark::prelude::nova::spartan::snark::RelaxedR1CSSNARK;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::r1cs;
use rings_snark::snark;

/// Bundled simple BN256 R1CS path.
pub const SIMPLE_BN256_R1CS: &str = "circoms/simple_bn256.r1cs";

/// Bundled simple BN256 witness wasm path.
pub const SIMPLE_BN256_WASM: &str = "circoms/simple_bn256.wasm";

/// Bundled Merkle-tree R1CS path.
pub const MERKLE_TREE_R1CS: &str = "circoms/merkle_tree.r1cs";

/// Bundled Merkle-tree witness wasm path.
pub const MERKLE_TREE_WASM: &str = "circoms/merkle_tree.wasm";

/// A result returned by runnable SNARK examples.
pub type ExampleResult<T> = Result<T, ExampleError>;

/// Errors returned by runnable SNARK examples.
#[derive(Debug)]
pub enum ExampleError {
    /// The underlying SNARK crate returned an error.
    Snark(rings_snark::error::Error),
    /// Recursive circuit generation returned no circuits.
    EmptyRecursiveCircuitSet,
}

impl fmt::Display for ExampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snark(error) => write!(f, "{error}"),
            Self::EmptyRecursiveCircuitSet => write!(f, "recursive circuit set is empty"),
        }
    }
}

impl error::Error for ExampleError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Snark(error) => Some(error),
            Self::EmptyRecursiveCircuitSet => None,
        }
    }
}

impl From<rings_snark::error::Error> for ExampleError {
    fn from(error: rings_snark::error::Error) -> Self {
        Self::Snark(error)
    }
}

/// Missing bundled circuit asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCircuitAsset {
    path: PathBuf,
}

impl MissingCircuitAsset {
    /// Return the missing path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for MissingCircuitAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "missing bundled circuit asset: {}", self.path.display())
    }
}

impl std::error::Error for MissingCircuitAsset {}

/// Return every circuit asset the examples load at runtime.
pub fn bundled_circuit_assets() -> [PathBuf; 4] {
    [
        snark_asset_path(SIMPLE_BN256_R1CS),
        snark_asset_path(SIMPLE_BN256_WASM),
        snark_asset_path(MERKLE_TREE_R1CS),
        snark_asset_path(MERKLE_TREE_WASM),
    ]
}

/// Resolve a bundled asset path relative to this example crate.
pub fn snark_asset_path(path: impl AsRef<Path>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

/// Resolve the simple BN256 R1CS asset path.
pub fn simple_bn256_r1cs_path() -> PathBuf {
    snark_asset_path(SIMPLE_BN256_R1CS)
}

/// Resolve the simple BN256 witness wasm asset path.
pub fn simple_bn256_wasm_path() -> PathBuf {
    snark_asset_path(SIMPLE_BN256_WASM)
}

/// Resolve the Merkle-tree R1CS asset path.
pub fn merkle_tree_r1cs_path() -> PathBuf {
    snark_asset_path(MERKLE_TREE_R1CS)
}

/// Resolve the Merkle-tree witness wasm asset path.
pub fn merkle_tree_wasm_path() -> PathBuf {
    snark_asset_path(MERKLE_TREE_WASM)
}

/// Verify that every bundled circuit asset exists.
pub fn verify_bundled_circuit_assets() -> Result<(), MissingCircuitAsset> {
    for asset in bundled_circuit_assets() {
        if !asset.is_file() {
            return Err(MissingCircuitAsset { path: asset });
        }
    }
    Ok(())
}

/// Return the first generated recursive circuit.
pub fn first_recursive_circuit<T>(circuits: &[T]) -> ExampleResult<&T> {
    circuits
        .first()
        .ok_or(ExampleError::EmptyRecursiveCircuitSet)
}

/// Public input used by the simple BN256 example.
pub fn simple_bn256_initial_input<F>() -> Input<F>
where F: PrimeField + From<u64> {
    vec![("step_in".to_string(), vec![F::from(4u64), F::from(2u64)])].into()
}

/// Private inputs used by the simple BN256 recursive example.
pub fn simple_bn256_private_inputs<F>() -> Vec<Input<F>>
where F: PrimeField + From<u64> {
    vec![
        vec![("adder".to_string(), vec![F::from(1u64)])].into(),
        vec![("adder".to_string(), vec![F::from(42u64)])].into(),
        vec![("adder".to_string(), vec![F::from(33u64)])].into(),
    ]
}

/// Execute the simple BN256 example's recursive prove-then-verify path once.
///
/// This is the cheapest end-to-end path for the example: it loads the bundled R1CS and
/// witness wasm, generates the initial circuit plus one recursive circuit, folds once,
/// and verifies the resulting recursive SNARK. Compression is intentionally left to the
/// heavier standalone example binary.
pub async fn simple_bn256_one_step_prove_verify() -> ExampleResult<()> {
    type E1 = VestaEngine;
    type E2 = PallasEngine;
    type EE1 = EvaluationEngine<E1>;
    type EE2 = EvaluationEngine<E2>;
    type S1 = RelaxedR1CSSNARK<E1, EE1>;
    type S2 = RelaxedR1CSSNARK<E2, EE2>;
    type F1 = <E1 as Engine>::Scalar;
    type F2 = <E2 as Engine>::Scalar;

    let r1cs = r1cs::load_r1cs::<F1>(
        r1cs::Path::Local(simple_bn256_r1cs_path().to_string_lossy().into_owned()),
        r1cs::Format::Bin,
    )
    .await?;
    let witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        simple_bn256_wasm_path().to_string_lossy().into_owned(),
    ))
    .await?;
    let circuit_generator =
        rings_snark::circuit::WasmCircuitGenerator::<F1>::new(r1cs, witness_calculator);

    let initial_input: Input<F1> = simple_bn256_initial_input();
    let private_inputs: Vec<Input<F1>> =
        simple_bn256_private_inputs().into_iter().take(1).collect();
    let public_input = vec![F1::from(4u64), F1::from(2u64)];
    let secondary_input = vec![F2::from(0u64)];
    let round = private_inputs.len();

    let initial_circuit = circuit_generator.gen_circuit(initial_input.clone(), true)?;
    let recursive_circuits =
        circuit_generator.gen_recursive_circuit(initial_input, private_inputs, round, true)?;
    let first_circuit = first_recursive_circuit(&recursive_circuits)?;

    let pp = snark::SNARK::<E1, E2>::gen_pp::<S1, S2>(initial_circuit)?;
    let mut recursive_snark =
        snark::SNARK::<E1, E2>::new(first_circuit, &pp, &public_input, &secondary_input)?;

    for circuit in recursive_circuits {
        recursive_snark.foldr(&pp, &circuit)?;
    }

    recursive_snark.verify(&pp, round, &public_input, &secondary_input)?;
    Ok(())
}

/// Public input used by the Merkle-tree example.
pub fn merkle_tree_initial_input<F>() -> Input<F>
where F: PrimeField + From<u64> {
    vec![("leaf".to_string(), vec![F::from(42u64)])].into()
}

/// Private path inputs used by the Merkle-tree example.
pub fn merkle_tree_private_inputs<F>() -> Vec<Input<F>>
where F: PrimeField + From<u64> {
    vec![
        vec![("path".to_string(), vec![F::from(1u64), F::from(0u64)])].into(),
        vec![("path".to_string(), vec![F::from(42u64), F::from(1u64)])].into(),
        vec![("path".to_string(), vec![F::from(33u64), F::from(0u64)])].into(),
    ]
}

#[cfg(test)]
mod tests {
    use rings_snark::prelude::nova::provider::PallasEngine;
    use rings_snark::prelude::nova::provider::VestaEngine;
    use rings_snark::prelude::nova::traits::Engine;

    use super::*;

    type Bn256TestField = <VestaEngine as Engine>::Scalar;
    type MerkleTreeTestField = <PallasEngine as Engine>::Scalar;

    #[test]
    fn bundled_circuit_assets_exist() {
        verify_bundled_circuit_assets().expect("bundled circuit assets");
    }

    #[test]
    fn simple_bn256_inputs_match_the_example_shape() {
        let initial = simple_bn256_initial_input::<Bn256TestField>();
        let private = simple_bn256_private_inputs::<Bn256TestField>();

        assert_eq!(initial.input.as_slice(), [("step_in".to_string(), vec![
            Bn256TestField::from(4u64),
            Bn256TestField::from(2u64)
        ])]);
        assert_eq!(private.len(), 3);
        assert!(private
            .iter()
            .all(|input| matches!(input.input.as_slice(), [(name, _)] if name == "adder")));
    }

    #[tokio::test]
    async fn simple_bn256_example_runs_one_step_prove_verify() {
        simple_bn256_one_step_prove_verify()
            .await
            .expect("simple BN256 one-step prove and verify");
    }

    #[test]
    fn merkle_tree_inputs_match_the_example_shape() {
        let initial = merkle_tree_initial_input::<MerkleTreeTestField>();
        let private = merkle_tree_private_inputs::<MerkleTreeTestField>();

        assert_eq!(initial.input.as_slice(), [("leaf".to_string(), vec![
            MerkleTreeTestField::from(42u64)
        ])]);
        assert_eq!(private.len(), 3);
        assert!(private.iter().all(
            |input| matches!(input.input.as_slice(), [(name, path)] if name == "path" && path.len() == 2)
        ));
    }
}
