use wasm_bindgen_test::wasm_bindgen_test;

use crate::error::Result;
use crate::prelude::nova::provider::VestaEngine;
use crate::prelude::nova::traits::Engine;
use crate::r1cs;

#[wasm_bindgen_test]
pub async fn test_wasm_load_witness_remote() -> Result<()> {
    type F = <VestaEngine as Engine>::Base;

    let url = "https://github.com/RingsNetwork/rings/raw/master/crates/snark/src/tests/circoms/simple_bn256.wasm";
    let mut witness_calculator =
        r1cs::load_circom_witness_calculator(r1cs::Path::Remote(url.to_string()))
            .await
            .unwrap();
    let input = vec![("step_in".to_string(), vec![F::from(4u64), F::from(2u64)])];
    let witness = witness_calculator
        .calculate_witness::<F>(input, true)
        .unwrap();
    assert_eq![witness[0], F::from(1u64)];
    Ok(())
}
