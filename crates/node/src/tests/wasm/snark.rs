use rings_snark::prelude::nova::provider;
use rings_snark::prelude::nova::traits::Engine;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::backend::snark::browser::bigint2ff;
use crate::backend::snark::Input;
use crate::backend::snark::SupportedPrimeField;

#[wasm_bindgen_test]
fn test_bigint2ff() {
    type F = <provider::VestaEngine as Engine>::Scalar;

    let bigint = js_sys::BigInt::new(&JsValue::from(42)).unwrap();
    let ff = bigint2ff::<F>(bigint).unwrap();
    assert_eq!(ff, F::from(42u64));
}

#[wasm_bindgen_test]
fn test_map_array_to_input() {
    let js_code_body = "return [['foo', [BigInt(2), BigInt(3)]], ['bar', [BigInt(4), BigInt(5)]]]";
    let func = js_sys::Function::new_no_args(js_code_body);
    let array = js_sys::Array::from(&func.call0(&JsValue::NULL).unwrap());
    let _input = Input::from_array(array, SupportedPrimeField::Vesta);
}
