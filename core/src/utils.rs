//! Utils for ring-core
use chrono::Utc;
/// Get local utc timestamp (millisecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(feature = "wasm")]
/// Toolset for wasm
pub mod js_value {
    use js_sys::Reflect;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde::Serializer;
    use wasm_bindgen::JsValue;

    use crate::error::Error;
    use crate::error::Result;

    /// Get property from a JsValue.
    pub fn get<T: DeserializeOwned>(obj: &JsValue, key: impl Into<String>) -> Result<T> {
        let key = key.into();
        let value = Reflect::get(obj, &JsValue::from(key.clone()))
            .map_err(|_| Error::FailedOnGetProperty(key.clone()))?;
        serde_wasm_bindgen::from_value(value).map_err(Error::SerdeWasmBindgenError)
    }

    /// Set Property to a JsValue.
    pub fn set(obj: &JsValue, key: impl Into<String>, value: impl Into<JsValue>) -> Result<bool> {
        let key = key.into();
        Reflect::set(obj, &JsValue::from(key.clone()), &value.into())
            .map_err(|_| Error::FailedOnSetProperty(key.clone()))
    }

    /// From serde to JsValue
    pub fn serialize(obj: &impl Serialize) -> Result<JsValue> {
        let serializer = serde_wasm_bindgen::Serializer::json_compatible();
        serializer
            .serialize_some(&obj)
            .map_err(Error::SerdeWasmBindgenError)
    }

    /// From JsValue to serde
    pub fn deserialize<T: DeserializeOwned>(obj: impl Into<JsValue>) -> Result<T> {
        serde_wasm_bindgen::from_value(obj.into()).map_err(Error::SerdeWasmBindgenError)
    }
}

#[cfg(feature = "wasm")]
pub mod js_func {
    /// This macro will generate a wrapper for mapping a js_sys::Function with type fn(T, T, T, T) -> Promise<()>
    /// to native function
    /// # Example:
    /// For macro calling: of!(of4, a: T0, b: T1, c: T2, d: T3);
    /// Will generate code:
    /// ```rust
    /// pub fn of4<
    ///     'a,
    ///     'b: 'a,
    ///     T0: TryInto<JsValue> + Clone,
    ///     T1: TryInto<JsValue> + Clone,
    ///     T2: TryInto<JsValue> + Clone,
    ///     T3: TryInto<JsValue> + Clone,
    /// >(
    ///     func: &Function,
    /// ) -> Box<dyn Fn(&'b T0, &'b T1, &'b T2, &'b T3) -> Pin<Box<dyn Future<Output = Result<()>> + 'b>>>
    /// where T0::Error: Debug,
    ///       T1::Error: Debug,
    ///       T2::Error: Debug,
    ///       T3::Error: Debug
    /// {
    ///     let func = func.clone();
    ///     Box::new(
    ///         move |a: &T0, b: &T1, c: &T2, d: &T3| -> Pin<Box<dyn Future<Output = Result<()>>>> {
    ///             let func = func.clone();
    ///             Box::pin(async move {
    ///                 let func = func.clone();
    ///                 JsFuture::from(js_sys::Promise::from(
    ///                     func.apply(
    ///                         &JsValue::NULL,
    ///                         &Array::from_iter(
    ///                             vec![
    ///                                 &a.clone().into(),
    ///                                 &b.clone().into(),
    ///                                 &c.clone().into(),
    ///                                 &d.clone().into(),
    ///                             ]
    ///                             .into_iter(),
    ///                         ),
    ///                     )
    ///                     .map_err(|e| Error::JsError(js_sys::Error::from(e).to_string().into()))?,
    ///                 ))
    ///                 .await
    ///                 .map_err(|e| Error::JsError(js_sys::Error::from(e).to_string().into()))?;
    ///                 Ok(())
    ///             })
    ///         },
    ///     )
    /// }
    /// ```
    #[macro_export]
    macro_rules! of {
	($func: ident, $($name:ident: $type: ident),+$(,)? ) => (
	    pub fn $func<'a, 'b: 'a, $($type: TryInto<wasm_bindgen::JsValue> + Clone),+>(
		func: &js_sys::Function,
	    ) -> Box<dyn Fn($(&'b $type),+) -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>> + 'b>>>
		where  $($type::Error: std::fmt::Debug),+
	    {
		let func = func.clone();
		Box::new(
		    move |$($name: &$type,)+| -> std::pin::Pin<Box<dyn std::future::Future<Output = $crate::error::Result<()>>>> {
			let func = func.clone();
			Box::pin(async move {
			    let func = func.clone();
			    let params = vec![
				$(&$name.clone().try_into().map_err(|e| $crate::error::Error::JsError(format!("{:?}", e)))?)+,
			    ];
			    tracing::info!("fucking params! {:?}", params.clone());
			    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(
				func.apply(
				    &wasm_bindgen::JsValue::NULL,
				    &js_sys::Array::from_iter(params.into_iter())
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
	)
    }

    of!(of1, a: T0);
    of!(of2, a: T0, b: T1);
    of!(of3, a: T0, b: T1, c: T2);
    of!(of4, a: T0, b: T1, c: T2, d: T3);
}

#[cfg(feature = "wasm")]
pub mod js_utils {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub enum Global {
        Window(web_sys::Window),
        WorkerGlobal(web_sys::WorkerGlobalScope),
        ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
    }

    pub fn global() -> Option<Global> {
        let obj = JsValue::from(js_sys::global());
        if obj.has_type::<web_sys::Window>() {
            return Some(Global::Window(web_sys::Window::from(obj)));
        }
        if obj.has_type::<web_sys::WorkerGlobalScope>() {
            return Some(Global::WorkerGlobal(web_sys::WorkerGlobalScope::from(obj)));
        }
        if obj.has_type::<web_sys::ServiceWorkerGlobalScope>() {
            return Some(Global::ServiceWorkerGlobal(
                web_sys::ServiceWorkerGlobalScope::from(obj),
            ));
        }
        None
    }

    pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
        let promise = match global().unwrap() {
            Global::Window(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
            Global::WorkerGlobal(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
            Global::ServiceWorkerGlobal(window) => js_sys::Promise::new(&mut |resolve, _| {
                let func = Closure::once_into_js(move || {
                    resolve.call0(&JsValue::NULL).unwrap();
                });
                window
                    .set_timeout_with_callback_and_timeout_and_arguments_0(
                        func.as_ref().unchecked_ref(),
                        millis,
                    )
                    .unwrap();
            }),
        };
        wasm_bindgen_futures::JsFuture::from(promise)
    }
}
