use rings_snark::prelude::nova::provider;
use rings_snark::prelude::nova::traits::Engine;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::backend::snark::browser::bigint2ff;

#[wasm_bindgen_test]
fn test_bigint2ff() {
    type F = <provider::VestaEngine as Engine>::Scalar;

    let bigint = js_sys::BigInt::new(&JsValue::from(42)).unwrap();
    let ff = bigint2ff::<F>(bigint).unwrap();
    assert_eq!(ff, F::from(42u64));
}
