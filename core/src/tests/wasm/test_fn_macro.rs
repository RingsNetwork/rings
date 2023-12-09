use js_sys::Array;
use js_sys::Function;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::utils::js_func;

#[wasm_bindgen_test]
async fn test_fn_generator() {
    let js_code_args = "a, b, c, d";
    let js_code_body = r#"
try {
    return new Promise((resolve, reject) => {
        const ret = a + b + c + d
        if (ret !== "hello world") {
          reject(`a: ${a}, b: ${b}, c: ${c}, d: ${d} -> ret: ${ret}`)
        } else {
          resolve(ret)
        }
    })
} catch(e) {
    return e
}
"#;
    let func = Function::new_with_args(js_code_args, &js_code_body);
    let native_func = js_func::of4::<String, String, String, String>(&func);
    let a = "hello".to_string();
    let b = " ".to_string();
    let c = "world".to_string();
    let d = "".to_string();
    native_func(a, b, c, d).await.unwrap();
}

#[wasm_bindgen_test]
async fn test_try_into() {
    let a = "hello".to_string();
    let b = " ".to_string();
    let c = "world".to_string();
    let p: Vec<JsValue> = vec![
        a.try_into().unwrap(),
        b.try_into().unwrap(),
        c.try_into().unwrap(),
    ];
    let array = Array::from_iter(p.into_iter());
    assert_eq!(array.to_vec().len(), 3, "{:?}", array.to_vec());
}
