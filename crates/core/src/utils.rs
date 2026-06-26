//! Utils for ring-core
use chrono::Utc;
/// Get local utc timestamp (millisecond)
pub fn get_epoch_ms() -> u128 {
    Utc::now().timestamp_millis() as u128
}

#[cfg(feature = "wasm")]
/// Toolset for wasm
pub mod js_value {
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde::Serializer;
    use wasm_bindgen::JsValue;

    use crate::error::Error;
    use crate::error::Result;

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

    /// From JsValue to serde_json::Value
    pub fn json_value(obj: impl Into<JsValue>) -> Result<serde_json::Value> {
        let s = js_sys::JSON::stringify(&obj.into())
            .map_err(|_| Error::JsError("failed to stringify obj".to_string()))?;

        serde_json::from_str(&String::from(s)).map_err(Error::Deserialize)
    }
}

#[cfg(feature = "wasm")]
pub mod js_func {
    /// This macro will generate a wrapper for mapping a js_sys::Function with type fn(T, T, T, T) -> Promise<()>
    /// to native function
    /// # Example:
    /// For macro calling: of!(of2, a: T0, b: T1);
    /// Will generate code:
    /// ```rust
    /// pub fn of2<'a, 'b: 'a, T0: TryInto<JsValue> + Clone, T1: TryInto<JsValue> + Clone>(
    ///     func: &Function,
    /// ) -> Box<dyn Fn(T0, T1) -> Pin<Box<dyn Future<Output = Result<()>> + 'b>>>
    /// where
    ///     T0: 'b,
    ///     T1: 'b,
    ///     T0::Error: Debug,
    ///     T1::Error: Debug,
    /// {
    ///     let func = func.clone();
    ///     Box::new(
    ///         move |a: T0, b: T1| -> Pin<Box<dyn Future<Output = Result<()>>>> {
    ///             let func = func.clone();
    ///             Box::pin(async move {
    ///                 let func = func.clone();
    ///                 let params = js_sys::Array::new();
    ///                 let a: JsValue = a
    ///                     .clone()
    ///                     .try_into()
    ///                     .map_err(|_| Error::JsError(format!("{:?}", e)));
    ///                 params.push(&a);
    ///                 let b: JsValue = b
    ///                     .clone()
    ///                     .try_into()
    ///                     .map_err(|_| Error::JsError(format!("{:?}", e)));
    ///                 params.push(&b);
    ///                 JsFuture::from(js_sys::Promise::from(
    ///                     func.apply(&JsValue::NULL, &params).map_err(|e| {
    ///                         Error::JsError(js_sys::Error::from(e).to_string().into())
    ///                     })?,
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
	($func: ident, $($name:ident: $type: ident),+$(,)?) => {
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
}

#[cfg(feature = "wasm")]
pub mod js_utils {
    use std::future::Future;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;
    use wasm_bindgen::JsValue;

    pub enum Global {
        Window(web_sys::Window),
        WorkerGlobal(web_sys::WorkerGlobalScope),
        ServiceWorkerGlobal(web_sys::ServiceWorkerGlobalScope),
    }

    impl Global {
        pub fn set_timeout_0(
            &self,
            callback: &js_sys::Function,
            millis: i32,
        ) -> Result<i32, JsValue> {
            match self {
                Global::Window(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::WorkerGlobal(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
                Global::ServiceWorkerGlobal(global) => {
                    global.set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
                }
            }
        }
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

    fn resolve_sleep(resolve: &js_sys::Function) {
        if let Err(error) = resolve.call0(&JsValue::NULL) {
            tracing::error!("Failed to resolve sleep promise: {:?}", error);
        }
    }

    fn reject_sleep(reject: &js_sys::Function, error: JsValue) {
        if let Err(reject_error) = reject.call1(&JsValue::NULL, &error) {
            tracing::error!("Failed to reject sleep promise: {:?}", reject_error);
        }
    }

    fn schedule_sleep<F>(resolve: js_sys::Function, reject: js_sys::Function, schedule: F)
    where F: FnOnce(&js_sys::Function) -> Result<i32, JsValue> {
        let func = Closure::once_into_js(move || {
            resolve_sleep(&resolve);
        });
        let callback = func.as_ref().unchecked_ref();
        if let Err(error) = schedule(callback) {
            tracing::error!("Failed to schedule sleep timeout: {:?}", error);
            reject_sleep(&reject, error);
        }
    }

    pub fn window_sleep(millis: i32) -> wasm_bindgen_futures::JsFuture {
        let promise = match global() {
            None => js_sys::Promise::reject(&JsValue::from_str("No global scope for window_sleep")),
            Some(global) => js_sys::Promise::new(&mut move |resolve, reject| {
                schedule_sleep(resolve, reject, |callback| {
                    global.set_timeout_0(callback, millis)
                });
            }),
        };
        wasm_bindgen_futures::JsFuture::from(promise)
    }

    /// Spawn a wasm-local interval loop that waits for each tick task to finish.
    pub fn spawn_interval<F, Fut>(millis: i32, mut task: F)
    where
        F: FnMut() -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                if let Err(error) = window_sleep(millis).await {
                    tracing::error!("failed to wait for interval tick: {:?}", error);
                    return;
                }

                task().await;
            }
        });
    }
}
