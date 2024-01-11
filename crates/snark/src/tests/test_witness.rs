use crate::r1cs;
use crate::prelude::nova::provider::VestaEngine;
use crate::error::Result;
use crate::prelude::nova::traits::Engine;

#[tokio::test]
pub async fn test_calcu_witness_sha256() -> Result<()>{
    type F = <VestaEngine as Engine>::Base;
    let r1cs = r1cs::load_r1cs_local::<F>("src/tests/circoms/sha256/circom_sha256.r1cs", r1cs::Format::Bin).unwrap();
    assert_eq!(r1cs.num_inputs, 4, "wrong inputs {:?}", r1cs.num_inputs);
    let mut witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        "src/tests/circoms/sha256/circom_sha256.wasm".to_string(),
    ))
	.await
	.unwrap();
    let input = vec![("arg_in".to_string(), vec![F::from(4u64), F::from(2u64)])];
    let witn = witness_calculator.calculate_witness::<F>(input, true).unwrap();
    assert_eq![witn[0], F::from(1u64)];
    // witness: <1> <Outputs> <Inputs> <Auxs>
    // test input
    assert_eq!(witn[2], F::from(4u64), "input is not included, top 10: {:?}", &witn[..10]);
    assert_eq!(witn[3], F::from(2u64), "input is not included, top 10: {:?}", &witn[..10]);
    Ok(())
}


#[tokio::test]
pub async fn test_calcu_witness_bn256() -> Result<()>{
    type F = <VestaEngine as Engine>::Base;
    let mut witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        "src/tests/circoms/simple_bn256.wasm".to_string(),
    ))
	.await
	.unwrap();
    let input = vec![("step_in".to_string(), vec![F::from(4u64), F::from(2u64)])];
    let witn = witness_calculator.calculate_witness::<F>(input, true).unwrap();
    assert_eq![witn[0], F::from(1u64)];
    // witness: <1> <Outputs> <Inputs> <Auxs>
    // test input
    assert_eq!(witn[3], F::from(4u64), "input is not included, {:?}", &witn);
    assert_eq!(witn[4], F::from(2u64), "input is not included, {:?}", &witn);
    Ok(())
}
