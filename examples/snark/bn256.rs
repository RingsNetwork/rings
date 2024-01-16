use std::rc::Rc;

use rings_snark::circuit;
use rings_snark::circuit::Input;
use rings_snark::prelude::nova::provider::ipa_pc::EvaluationEngine;
use rings_snark::prelude::nova::provider::PallasEngine;
use rings_snark::prelude::nova::provider::VestaEngine;
use rings_snark::prelude::nova::spartan::snark::RelaxedR1CSSNARK;
use rings_snark::prelude::nova::traits::Engine;
use rings_snark::r1cs;
use rings_snark::snark;

#[tokio::main]
async fn main() {
    type E1 = VestaEngine;
    type E2 = PallasEngine;
    type EE1 = EvaluationEngine<E1>;
    type EE2 = EvaluationEngine<E2>;
    type S1 = RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
    type S2 = RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
    type F1 = <E1 as Engine>::Scalar;
    type F2 = <E2 as Engine>::Scalar;

    let r1cs = r1cs::load_r1cs::<F1>(
        r1cs::Path::Local("examples/snark/circoms/simple_bn256.r1cs".to_string()),
        r1cs::Format::Bin,
    )
    .await
    .unwrap();
    let witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        "examples/snark/circoms/simple_bn256.wasm".to_string(),
    ))
    .await
    .unwrap();

    let circuit_generator = circuit::WasmCircuitGenerator::<F1>::new(r1cs, witness_calculator);

    let input_0: Input<F1> =
        vec![("step_in".to_string(), vec![F1::from(4u64), F1::from(2u64)])].into();
    let private_inputs: Vec<Input<F1>> = vec![
        vec![("adder".to_string(), vec![F1::from(1u64)])].into(),
        vec![("adder".to_string(), vec![F1::from(42u64)])].into(),
        vec![("adder".to_string(), vec![F1::from(33u64)])].into(),
    ];
    assert_eq!(private_inputs.len(), 3);

    let circuit_0 = circuit_generator
        .gen_circuit(input_0.clone(), true)
        .unwrap();

    let recursive_circuits = circuit_generator
        .gen_recursive_circuit(input_0.clone(), private_inputs.clone(), 3, true)
        .unwrap();

    assert_eq!(recursive_circuits.len(), 3);
    // init pp with ouptn inputs
    let pp = snark::SNARK::<E1, E2>::gen_pp::<S1, S2>(circuit_0.clone());
    let mut rec_snark_iter =
        snark::SNARK::<E1, E2>::new(&recursive_circuits[0].clone(), &pp, input_0.clone(), vec![
            F2::from(0),
        ])
        .unwrap();

    for c in recursive_circuits {
        rec_snark_iter.foldr(&pp, &c).unwrap();
    }
    rec_snark_iter
        .verify(&pp, 3, &vec![F1::from(4u64), F1::from(2u64)], &vec![
            F2::from(0),
        ])
        .unwrap();
    println!("success on create recursive snark");
    let (pk, vk) = snark::SNARK::<E1, E2>::compress_setup::<S1, S2>(&pp).unwrap();

    let compress_snark = rec_snark_iter.compress_prove::<S1, S2>(&pp, &pk).unwrap();
    let compress_snark_ref = Rc::new(compress_snark);
    let ret = snark::SNARK::<E1, E2>::compress_verify::<S1, S2>(compress_snark_ref, &vk, 3, &vec![
        F1::from(4u64),
        F1::from(2u64),
    ]);
    assert!(ret.is_ok());
}
