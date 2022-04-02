/// for impl recursion, we need:
/// func = fn(func: Function) {
///     poll();
///     set_timeout(func, timeout, func);
/// }
/// set_timeout(func, timeout, func)
#[macro_export]
macro_rules! poll {
    ( $func:expr, $ttl:expr ) => {{
        use wasm_bindgen::JsCast;
        let window = web_sys::window().unwrap();
        let func = wasm_bindgen::prelude::Closure::wrap(
            (box move |func: js_sys::Function| {
                $func();
                let window = web_sys::window().unwrap();
                window
                    .set_timeout_with_callback_and_timeout_and_arguments(
                        func.unchecked_ref(),
                        $ttl,
                        &js_sys::Array::of1(&func),
                    )
                    .unwrap();
            }) as Box<dyn FnMut(js_sys::Function)>,
        );
        window
            .set_timeout_with_callback_and_timeout_and_arguments(
                &func.as_ref().unchecked_ref(),
                $ttl,
                &js_sys::Array::of1(&func.as_ref().unchecked_ref()),
            )
            .unwrap();
        func.forget();
    }};
}
