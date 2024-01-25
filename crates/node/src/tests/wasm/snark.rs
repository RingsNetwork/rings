use rings_snark::prelude::nova::provider;
use rings_snark::prelude::nova::traits::Engine;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;
use crate::backend::snark::SupportedPrimeField;
use crate::backend::snark::browser::bigint2ff;
use crate::backend::snark::Input;
use wasm_bindgen_test::*;
use crate::prelude::rings_core::utils::js_utils;
use wasm_bindgen_futures::JsFuture;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn test_bigint2ff() {
    type F = <provider::PallasEngine as Engine>::Scalar;

    let bigint = js_sys::BigInt::new(&JsValue::from(42)).unwrap();
    let ff = bigint2ff::<F>(bigint).unwrap();
    assert_eq!(ff, F::from(42u64));
}

#[wasm_bindgen_test]
fn test_map_array_to_input() {
    let js_code_body = "return [['foo', [BigInt(2), BigInt(3)]], ['bar', [BigInt(4)]]]";
    let func = js_sys::Function::new_no_args(js_code_body);
    let array = js_sys::Array::from(&func.call0(&JsValue::NULL).unwrap());
    let _input = Input::from_array(array, SupportedPrimeField::Pallas);
}


#[wasm_bindgen_test]
async fn test_send_snark_backend_message() {
    let provider1 = super::new_provider().await;
    let provider2 = super::new_provider().await;

    futures::try_join!(
        JsFuture::from(provider1.listen()),
        JsFuture::from(provider2.listen()),
    )
	.unwrap();
    // let wasm = "https://github.com/RingsNetwork/rings/raw/master/crates/snark/src/tests/circoms/simple_bn256.wasm";
    // let r1cs = "https://github.com/RingsNetwork/rings/raw/master/crates/snark/src/tests/circoms/simple_bn256.r1cs";

    super::create_connection(&provider1, &provider2).await;
    console_log!("wait for register");
    js_utils::window_sleep(1000).await.unwrap();

    let r1cs = "http://localhost:7777/merkle_tree.r1cs";
    let wasm = "http://localhost:7777/merkle_tree.wasm";

    let snark_behaviour = crate::backend::snark::SNARKBehaviour::new_instance();
    let snark_task_builder = crate::backend::snark::SNARKTaskBuilder::from_remote(
	r1cs.to_string(),
	wasm.to_string(),
	crate::backend::snark::SupportedPrimeField::Pallas
    ).await.unwrap();
    type F = crate::backend::snark::Field;
    let input: Input = vec![
	("leaf".to_string(), vec![F::from_u64(4u64, SupportedPrimeField::Pallas)])
    ].into();
    console_log!("gen circuit");
    let circuits = snark_task_builder.gen_circuits(input, vec![], 5).unwrap();

    let promise = snark_behaviour.send_proof_task_to(provider1, circuits, provider2.address());
    wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();
}
