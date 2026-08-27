/// This macro will generate a wrapper for mapping a js_sys::Function with type fn(T, T, T, T) -> Promise<()>
/// to native function
/// # Example:
/// For macro calling: of!(of2, a: T0, b: T1);
/// Will generate code:
/// ```rust,no_run
/// pub fn of2<
///     'a,
///     'b: 'a,
///     T0: TryInto<wasm_bindgen::JsValue> + Clone,
///     T1: TryInto<wasm_bindgen::JsValue> + Clone,
/// >(
///     func: &js_sys::Function,
/// ) -> Box<
///     dyn Fn(
///         T0,
///         T1,
///     ) -> std::pin::Pin<
///         Box<dyn std::future::Future<Output = rings_core::error::Result<()>> + 'b>,
///     >,
/// >
/// where
///     T0::Error: std::fmt::Debug,
///     T1::Error: std::fmt::Debug,
///     T0: 'b,
///     T1: 'b,
/// {
///     let func = func.clone();
///     Box::new(
///         move |a: T0,
///               b: T1|
///               -> std::pin::Pin<
///             Box<dyn std::future::Future<Output = rings_core::error::Result<()>>>,
///         > {
///             let func = func.clone();
///             Box::pin(async move {
///                 let func = func.clone();
///                 let params = js_sys::Array::new();
///                 let a: wasm_bindgen::JsValue = a
///                     .clone()
///                     .try_into()
///                     .map_err(|e| rings_core::error::Error::JsError(format!("{:?}", e)))?;
///                 params.push(&a);
///                 let b: wasm_bindgen::JsValue = b
///                     .clone()
///                     .try_into()
///                     .map_err(|e| rings_core::error::Error::JsError(format!("{:?}", e)))?;
///                 params.push(&b);
///                 wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(
///                     func.apply(&wasm_bindgen::JsValue::NULL, &params)
///                         .map_err(|e| rings_core::error::Error::from(js_sys::Error::from(e)))?,
///                 ))
///                 .await
///                 .map_err(|e| rings_core::error::Error::from(js_sys::Error::from(e)))?;
///                 Ok(())
///             })
///         },
///     )
/// }
/// ```
#[macro_export]
macro_rules! of {
	($func: ident, $($name:ident: $type: ident),+$(,)?) => {
        #[doc = "Wrap a JavaScript function in an async Rust callback."]
	    pub fn $func<'a, 'b: 'a, $($type: TryInto<wasm_bindgen::JsValue> + Clone),+>(
	        func: &js_sys::Function,
	    ) -> Box<dyn Fn($($type),+) -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>> + 'b>>>
	    where  $($type::Error: std::fmt::Debug),+,
		$($type: 'b),+
	    {
		let func = func.clone();
		Box::new(
		    move |$($name: $type,)+| -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>>>> {
			let func = func.clone();
			Box::pin(async move {
			    let func = func.clone();
			    let params = js_sys::Array::new();
			    $(
				let $name: wasm_bindgen::JsValue = $name.clone().try_into().map_err(|e| $crate::error::Error::JsError(format!("{:?}", e)))?;
				params.push(&$name);
			    )+
			    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(
				func.apply(
				    &wasm_bindgen::JsValue::NULL,
				    &params
				)
				    .map_err(|e| $crate::error::Error::from(js_sys::Error::from(e)))?,
			    ))
				.await
				.map_err(|e| $crate::error::Error::from(js_sys::Error::from(e)))?;
			    Ok(())
			})
		    },
		)
	    }
	}
}

of!(of1, a: T0);
of!(of2, a: T0, b: T1);
of!(of3, a: T0, b: T1, c: T2);
of!(of4, a: T0, b: T1, c: T2, d: T3);
