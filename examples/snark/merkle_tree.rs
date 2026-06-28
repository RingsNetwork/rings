use std::time::Instant;

use rings_snark::circuit;
use rings_snark::circuit::Input;
use rings_snark::prelude::nova::provider::ipa_pc::EvaluationEngine;
use rings_snark::prelude::nova::provider::PallasEngine;
use rings_snark::prelude::nova::provider::VestaEngine;
use rings_snark::prelude::nova::spartan::snark::RelaxedR1CSSNARK;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::r1cs;
use rings_snark::snark;
use rings_snark_example::first_recursive_circuit;
use rings_snark_example::merkle_tree_initial_input;
use rings_snark_example::merkle_tree_private_inputs;
use rings_snark_example::merkle_tree_r1cs_path;
use rings_snark_example::merkle_tree_wasm_path;
use rings_snark_example::ExampleResult;

pub async fn merkle_tree_path_proof() -> ExampleResult<()> {
    type E1 = PallasEngine;
    type E2 = VestaEngine;
    type EE1 = EvaluationEngine<E1>;
    type EE2 = EvaluationEngine<E2>;
    type S1 = RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
    type S2 = RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
    type F1 = <E1 as Engine>::Scalar;
    type F2 = <E2 as Engine>::Scalar;

    // load r1cs and wasm
    let start = Instant::now();

    let r1cs = r1cs::load_r1cs::<F1>(
        r1cs::Path::Local(merkle_tree_r1cs_path().to_string_lossy().into_owned()),
        r1cs::Format::Bin,
    )
    .await?;
    let witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        merkle_tree_wasm_path().to_string_lossy().into_owned(),
    ))
    .await?;

    println!("load r1cs and wasm, took {:?} ", start.elapsed());

    let circuit_generator = circuit::WasmCircuitGenerator::<F1>::new(r1cs, witness_calculator);
    let input_0: Input<F1> = merkle_tree_initial_input();
    let input_value_0 = vec![F1::from(42u64)];
    let private_inputs: Vec<Input<F1>> = merkle_tree_private_inputs();

    let round = private_inputs.len();
    println!("total round {round:?}");
    // Gen recursived circuit
    let start = Instant::now();

    let circuit_0 = circuit_generator.gen_circuit(input_0.clone(), true)?;
    let first_input = circuit_0.get_public_inputs();

    let recursive_circuits = circuit_generator.gen_recursive_circuit(
        input_0.clone(),
        private_inputs.clone(),
        round,
        true,
    )?;

    println!("gen recursived circuit, took {:?} ", start.elapsed());

    let start = Instant::now();
    // init pp with ouptn inputs
    let pp = snark::SNARK::<E1, E2>::gen_pp::<S1, S2>(circuit_0.clone())?;
    println!("gen public parasm, took {:?} ", start.elapsed());

    let start = Instant::now();
    let first_circuit = first_recursive_circuit(&recursive_circuits)?;
    let mut rec_snark_iter =
        snark::SNARK::<E1, E2>::new(first_circuit, &pp, &first_input, vec![F2::from(0)])?;

    for c in &recursive_circuits {
        rec_snark_iter.foldr(&pp, &c)?;
    }
    println!("fold all circuit, took {:?} ", start.elapsed());
    println!("success on create recursive snark");

    let start = Instant::now();
    let (pk, vk) = snark::SNARK::<E1, E2>::compress_setup::<S1, S2>(&pp)?;
    println!("compressed snark setup, took {:?} ", start.elapsed());

    let start = Instant::now();
    let compress_snark = rec_snark_iter.compress_prove::<S1, S2>(&pp, &pk)?;
    println!("compressed snark proof, took {:?} ", start.elapsed());

    let start = Instant::now();
    snark::SNARK::<E1, E2>::compress_verify::<S1, S2>(&compress_snark, &vk, round, &input_value_0)?;
    println!("compressed snark verify, took {:?} ", start.elapsed());

    Ok(())
}

#[tokio::main]
async fn main() -> ExampleResult<()> {
    merkle_tree_path_proof().await
}
