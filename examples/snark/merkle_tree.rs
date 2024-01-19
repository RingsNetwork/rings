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

pub async fn merkle_tree_path_proof() {
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
        r1cs::Path::Local("examples/snark/circoms/merkle_tree.r1cs".to_string()),
        r1cs::Format::Bin,
    )
    .await
    .unwrap();
    let witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        "examples/snark/circoms/merkle_tree.wasm".to_string(),
    ))
    .await
    .unwrap();

    println!("load r1cs and wasm, took {:?} ", start.elapsed());

    let circuit_generator = circuit::WasmCircuitGenerator::<F1>::new(r1cs, witness_calculator);
    let input_value_0 = vec![F1::from(42u64)];
    let input_0: Input<F1> = vec![("leaf".to_string(), input_value_0.clone())].into();
    let private_inputs: Vec<Input<F1>> = vec![
        // element, index
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(42u64), F1::from(1u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(1u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(33u64), F1::from(0u64)])].into(),
        vec![("path".to_string(), vec![F1::from(77777u64), F1::from(0u64)])].into(),
    ];

    let round = private_inputs.len() - 1;
    println!("total round {:?}", round);
    // Gen recursived circuit
    let start = Instant::now();

    let circuit_0 = circuit_generator
        .gen_circuit(input_0.clone(), true)
        .unwrap();

    let recursive_circuits = circuit_generator
        .gen_recursive_circuit(input_0.clone(), private_inputs.clone(), round, true)
        .unwrap();

    println!("gen recursived circuit, took {:?} ", start.elapsed());

    let start = Instant::now();
    // init pp with ouptn inputs
    let pp = snark::SNARK::<E1, E2>::gen_pp::<S1, S2>(circuit_0.clone());
    println!("gen public parasm, took {:?} ", start.elapsed());

    let start = Instant::now();
    let mut rec_snark_iter =
        snark::SNARK::<E1, E2>::new(&recursive_circuits[0].clone(), &pp, input_0.clone(), vec![
            F2::from(0),
        ])
        .unwrap();

    for c in &recursive_circuits {
        rec_snark_iter.foldr(&pp, &c).unwrap();
    }
    println!("fold all circuit, took {:?} ", start.elapsed());
    // rec_snark_iter
    //     .verify(&pp, round, &input_value_0, &vec![
    //         F2::from(0),
    //     ])
    //     .unwrap();
    println!("success on create recursive snark");

    let start = Instant::now();
    let (pk, vk) = snark::SNARK::<E1, E2>::compress_setup::<S1, S2>(&pp).unwrap();
    println!("compressed snark setup, took {:?} ", start.elapsed());

    let start = Instant::now();
    let compress_snark = rec_snark_iter.compress_prove::<S1, S2>(&pp, &pk).unwrap();
    println!("compressed snark proof, took {:?} ", start.elapsed());

    let start = Instant::now();
    let ret = snark::SNARK::<E1, E2>::compress_verify::<S1, S2>(
        &compress_snark,
        &vk,
        round,
        &input_value_0,
    );
    assert!(ret.is_ok());
    println!("compressed snark verify, took {:?} ", start.elapsed());
}

#[tokio::main]
async fn main() {
    merkle_tree_path_proof().await;
}
