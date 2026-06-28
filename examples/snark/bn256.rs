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
use rings_snark_example::first_recursive_circuit;
use rings_snark_example::simple_bn256_initial_input;
use rings_snark_example::simple_bn256_private_inputs;
use rings_snark_example::simple_bn256_r1cs_path;
use rings_snark_example::simple_bn256_wasm_path;
use rings_snark_example::ExampleResult;

#[tokio::main]
async fn main() -> ExampleResult<()> {
    type E1 = VestaEngine;
    type E2 = PallasEngine;
    type EE1 = EvaluationEngine<E1>;
    type EE2 = EvaluationEngine<E2>;
    type S1 = RelaxedR1CSSNARK<E1, EE1>; // non-preprocessing SNARK
    type S2 = RelaxedR1CSSNARK<E2, EE2>; // non-preprocessing SNARK
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

    let circuit_generator = circuit::WasmCircuitGenerator::<F1>::new(r1cs, witness_calculator);

    let input_0: Input<F1> = simple_bn256_initial_input();
    let private_inputs: Vec<Input<F1>> = simple_bn256_private_inputs();
    assert_eq!(private_inputs.len(), 3);

    let circuit_0 = circuit_generator.gen_circuit(input_0.clone(), true)?;

    let recursive_circuits = circuit_generator.gen_recursive_circuit(
        input_0.clone(),
        private_inputs.clone(),
        3,
        true,
    )?;

    assert_eq!(recursive_circuits.len(), 3);
    // init pp with ouptn inputs
    let pp = snark::SNARK::<E1, E2>::gen_pp::<S1, S2>(circuit_0.clone())?;
    let first_circuit = first_recursive_circuit(&recursive_circuits)?;
    let mut rec_snark_iter = snark::SNARK::<E1, E2>::new(
        first_circuit,
        &pp,
        vec![F1::from(4u64), F1::from(2u64)],
        vec![F2::from(0)],
    )?;

    for c in recursive_circuits {
        rec_snark_iter.foldr(&pp, &c)?;
    }
    rec_snark_iter.verify(&pp, 3, &vec![F1::from(4u64), F1::from(2u64)], &vec![
        F2::from(0),
    ])?;
    println!("success on create recursive snark");
    let (pk, vk) = snark::SNARK::<E1, E2>::compress_setup::<S1, S2>(&pp)?;

    let compress_snark = rec_snark_iter.compress_prove::<S1, S2>(&pp, &pk)?;
    let compress_snark_ref = Rc::new(compress_snark);
    snark::SNARK::<E1, E2>::compress_verify::<S1, S2>(compress_snark_ref, &vk, 3, &vec![
        F1::from(4u64),
        F1::from(2u64),
    ])?;

    Ok(())
}
