use crate::backend::snark::*;

#[tokio::test]
pub async fn test_gen_proof_and_verify() {
    let wasm = "../snark/src/tests/native/circoms/simple_bn256.wasm";
    let r1cs = "../snark/src/tests/native/circoms/simple_bn256.r1cs";
    let snark_task_builder = SNARKTaskBuilder::from_local(
        r1cs.to_string(),
        wasm.to_string(),
        crate::backend::snark::SupportedPrimeField::Vesta,
    )
    .await
    .unwrap();
    type F = crate::backend::snark::Field;
    let input: Input = vec![("step_in".to_string(), vec![
        F::from_u64(4u64, SupportedPrimeField::Vesta),
        F::from_u64(2u64, SupportedPrimeField::Vesta),
    ])]
    .into();
    let circuits = snark_task_builder.gen_circuits(input, vec![], 5).unwrap();
    assert_eq!(circuits.len(), 5);
    let task = SNARKBehaviour::gen_proof_task(circuits).unwrap();
    let proof = SNARKBehaviour::handle_snark_proof_task(&task).unwrap();
    let ret = SNARKBehaviour::handle_snark_verify_task(&proof, &task).unwrap();
    assert!(ret)
}
