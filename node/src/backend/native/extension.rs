#![warn(missing_docs)]
//! This module supports a user-defined message handler based on WebAssembly (Wasm).
//! The Rings network allows loading a Wasm or Wat file from a remote or local file and transforming it into a message handler.
//! The Wasm module should satisfy the following requirements:
//!
//! 1. It should have a function with the signature fn handler(param: ExternRef) -> ExternRef, and this function should be exported.
//!
//! 2. The Wasm module should not have any external imports, except for the helper functions defined by the Rings network.
//!
//! 3. Only the helper functions defined by the Rings network can be used, which include:
//!
//! ```text
//!     "message_abi" => {
//!         "request"  => request,
//!     }
//! ```
//! A basic wasm extension may looks like:
//!
//! ```wat
//! (module
//!  ;; Let's import message_type from message_abi
//!  (type $ty_request_type (func (param externref i32 i32) (result i32)))
//!  (import "request" "request_type" (func request_type (type $ty_message_type)))
//!   ;; fn handler(param: ExternRef) -> ExternRef
//!  (func $handler  (param externref) (result externref)
//!      (return (local.get 0))
//!  )
//!  (export "handler" (func $handler))
//! )
//!
//! You can see that this wat/wasm extension defines a handler function and
//! imports the request ABI.
use std::sync::Arc;
use std::sync::RwLock;

use loader::Handler;
use reqwest;
use serde::Deserialize;
use serde::Serialize;

use super::MessageHandler;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::*;
use crate::provider::Provider;

/// Path of a wasm extension
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// Local filesystem path
    Local(String),
    /// A remote resource needs to fetch
    Remote(String),
}

/// Configure for Extension
#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionConfig {
    /// Path of extension, can be remote or local
    pub paths: Vec<Path>,
}

/// Manager of Extension
pub struct Extension {
    /// Extension list
    handlers: Vec<Handler>,
}

/// Calls the extension handler with the given message and returns the response.
pub trait ExtensionHandlerCaller {
    /// Call extension handler
    fn call(&self, msg: bytes::Bytes, provider: Arc<Provider>) -> Result<()>;
}

/// Wrapper for BackendMessage and Provider that can be converted from and to the native WebAssembly type.
#[derive(Clone, Default)]
pub struct WasmABIContainer {
    msg: Arc<RwLock<Option<Box<bytes::Bytes>>>>,
    provider: Arc<RwLock<Option<Arc<Provider>>>>,
}

impl WasmABIContainer {
    /// Ask the instance to setup a new backend message to memory;
    pub fn set_message(&self, msg: bytes::Bytes) -> Result<()> {
        let mut guard = self
            .msg
            .write()
            .map_err(|_| Error::WasmBackendMessageRwLockError)?;
        *guard = Some(Box::new(msg));
        Ok(())
    }

    /// Ask the instance to set up provider for message calling
    pub fn set_provider(&self, provider: Arc<Provider>) -> Result<()> {
        let mut guard = self
            .provider
            .write()
            .map_err(|_| Error::WasmBackendMessageRwLockError)?;
        *guard = Some(provider);
        Ok(())
    }

    /// Create a new WasmAbiContainer instance
    pub fn new(msg: Option<bytes::Bytes>, provider: Arc<Provider>) -> Self {
        Self {
            msg: Arc::new(RwLock::new(msg.map(Box::new))),
            provider: Arc::new(RwLock::new(Some(provider))),
        }
    }
}

impl Extension {
    /// Loads a wasm module from the specified path, the path can be remote or local
    async fn load(path: &Path) -> Result<Handler> {
        match path {
            Path::Local(path) => loader::load_from_fs(path.to_string()).await,
            Path::Remote(path) => {
                let data: String = reqwest::get(path)
                    .await
                    .map_err(|e| Error::HttpRequestError(e.to_string()))?
                    .text()
                    .await
                    .map_err(|e| Error::HttpRequestError(e.to_string()))?;
                loader::load(data).await
            }
        }
    }

    /// Creates a new Extension instance with the specified configuration.
    pub async fn new(config: &ExtensionConfig) -> Result<Self> {
        let mut handlers = vec![];
        for p in &config.paths {
            if let Ok(h) = Self::load(p).await {
                handlers.push(h)
            } else {
                log::error!("Failed on loading extension {:?}", p)
            }
        }
        Ok(Self { handlers })
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MessageHandler<bytes::Bytes> for Extension {
    /// Handles the incoming message by passing it to the extension handlers and returning the resulting events.
    async fn handle_message(
        &self,
        provider: Arc<Provider>,
        _ctx: &MessagePayload,
        data: &bytes::Bytes,
    ) -> Result<()> {
        for h in &self.handlers {
            h.call(data.clone(), provider.clone())?;
        }
        Ok(())
    }
}

/// Loader of wasm, including ABI generator
pub mod loader {
    //! Wasm Loader module
    use core::any::Any;
    use std::ffi::CStr;
    use std::fs;
    use std::os::raw::c_char;
    use std::sync::Arc;
    use std::sync::RwLock;

    use lazy_static::lazy_static;
    use wasmer::imports;
    use wasmer::AsStoreMut;
    use wasmer::ExternRef;
    use wasmer::FromToNativeWasmType;
    use wasmer::FunctionEnv;
    use wasmer::FunctionEnvMut;
    use wasmer::FunctionType;
    use wasmer::Type;
    use wasmer::TypedFunction;
    use wasmer::Value;

    use super::WasmABIContainer;
    use crate::error::Error;
    use crate::error::Result;
    use crate::provider::Provider;

    lazy_static! {
        static ref WASM_MEM: Arc<RwLock<wasmer::Store>> =
            Arc::new(RwLock::new(wasmer::Store::default()));
    }

    impl TryFrom<WasmABIContainer> for FunctionEnv<WasmABIContainer> {
        type Error = Error;
        fn try_from(data: WasmABIContainer) -> Result<Self> {
            let mut mem = WASM_MEM
                .write()
                .map_err(|_| Error::WasmGlobalMemoryLockError)?;
            Ok(FunctionEnv::new(&mut mem, data))
        }
    }

    /// The "WasmABILander" defines how a Rust native struct generates the corresponding Wasm ABI for its getter functions.
    pub trait WasmABILander: Sized + Any + Send + 'static {
        /// The land_abi function needs to return an ImportObject.
        /// read more: <https://developer.mozilla.org/en-US/docs/WebAssembly/JavaScript_interface/instantiate>
        fn land_abi(env: &FunctionEnv<Self>, store: &mut impl AsStoreMut) -> wasmer::Imports;
    }

    impl WasmABILander for WasmABIContainer {
        fn land_abi(env: &FunctionEnv<Self>, store: &mut impl AsStoreMut) -> wasmer::Imports {
            let request = wasmer::Function::new_with_env(
                store,
                env,
                FunctionType::new(vec![Type::ExternRef, Type::I32, Type::I32], vec![]),
                WasmABIContainer::request,
            );

            #[rustfmt::skip]
            imports! {
		"message_abi" => {
                    "request" => request
                }
            }
        }
    }

    impl WasmABIContainer {
        /// wasm function type `Fn (Option<ExternalRef>, i32, i32) -> \[0\]`, external ref is always pointed to Env
        pub fn request(
            env: FunctionEnvMut<WasmABIContainer>,
            params: &[Value],
        ) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match params {
                [Value::ExternRef(_), Value::I32(method_ptr), Value::I32(params_ptr)] => {
                    let container: WasmABIContainer = env.data().clone();
                    let method_cstr = unsafe { CStr::from_ptr((*method_ptr) as *const c_char) };
                    let method = method_cstr.to_str().unwrap();
                    let params_cstr = unsafe { CStr::from_ptr((*params_ptr) as *const c_char) };
                    let params = params_cstr.to_str().unwrap();
                    let guard = container.provider.read().map_err(|_| {
                        wasmer::RuntimeError::new("Failed on lock memory of external ref")
                    })?;
                    if let Some(provider) = guard.clone() {
                        let params = serde_json::from_str(params)
                            .map_err(|_| wasmer::RuntimeError::new("Failed to serialize params"))?;
                        futures::executor::block_on(provider.request_internal(
                            method.to_string(),
                            params,
                            None,
                        ))
                        .map_err(|_| {
                            wasmer::RuntimeError::new("Failed to call async request function")
                        })?;
                    } else {
                        return Err(wasmer::RuntimeError::new(
                            "Failed on call func `write_at`:: ExternalRef is NULL",
                        ));
                    }
                    Ok(vec![])
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect [Externef, i32], got {:?}",
                    x
                ))),
            }
        }
    }

    unsafe impl FromToNativeWasmType for WasmABIContainer {
        type Native = Option<ExternRef>;

        fn from_native(native: Self::Native) -> Self {
            if native.is_none() {
                return Self::default();
            }
            match WASM_MEM
                .read()
                .map_err(|_| Error::WasmGlobalMemoryLockError)
            {
                Ok(mem) => {
                    if let Some(m) = native.unwrap().downcast::<Self>(&mem) {
                        m.clone()
                    } else {
                        Self::default()
                    }
                }
                Err(e) => {
                    log::error!("{:?}", e);
                    Self::default()
                }
            }
        }

        fn to_native(self) -> Self::Native {
            // Convert BackendMessage to the native representation
            match WASM_MEM
                .write()
                .map_err(|_| Error::WasmGlobalMemoryLockError)
            {
                Ok(mut mem) => {
                    let ext_ref = ExternRef::new::<Self>(&mut mem, self);
                    // Checks whether this ExternRef can be used with the given context.
                    if ext_ref.is_from_store(&mem) {
                        Some(ext_ref)
                    } else {
                        None
                    }
                }
                Err(e) => {
                    log::error!("{:?}", e);
                    None
                }
            }
        }
    }

    /// Type of message handler that the Wasm/Wat should implement
    type TyHandler = TypedFunction<Option<ExternRef>, Option<ExternRef>>;

    /// Externref type handler, this is a wrapper of handler function
    pub struct Handler {
        /// The native function get from wasm.
        pub func: TyHandler,
        /// By default, when resolving an ExternRef, it points to the function environment.
        pub container: WasmABIContainer,
    }

    impl super::ExtensionHandlerCaller for Handler {
        fn call(&self, msg: bytes::Bytes, provider: Arc<Provider>) -> Result<()> {
            self.container.set_message(msg)?;
            self.container.set_provider(provider)?;
            let native_container = self.container.clone().to_native();
            {
                let mut mem = WASM_MEM
                    .write()
                    .map_err(|_| Error::WasmGlobalMemoryLockError)?;
                self.func
                    .call(&mut mem, native_container)
                    .map_err(|e| Error::WasmRuntimeError(e.to_string()))?
            };
            Ok(())
        }
    }

    /// wasm loarder, bytes can be WAT of *.wasm binary
    pub async fn load(bytes: impl AsRef<[u8]>) -> Result<Handler> {
        let container = WasmABIContainer::default();
        let env: FunctionEnv<WasmABIContainer> = container.clone().try_into()?;
        let mut store = WASM_MEM
            .write()
            .map_err(|_| Error::WasmGlobalMemoryLockError)?;
        let module = wasmer::Module::new(&store, &bytes)
            .map_err(|e| Error::WasmCompileError(e.to_string()))?;
        // The module doesn't import anything, so we create an empty import object.
        let import_object = WasmABIContainer::land_abi(&env, &mut store);
        let ins = wasmer::Instance::new(&mut store, &module, &import_object)
            .map_err(|_| Error::WasmInstantiationError)?;
        let exports: wasmer::Exports = ins.exports;
        let handler: TyHandler = exports
            .get_function("handler")
            .map_err(|_| Error::WasmExportError)?
            .typed(&store)
            .map_err(|_| Error::WasmExportError)?;

        Ok(Handler {
            func: handler,
            container,
        })
    }

    /// Load wasm from filesystem
    pub async fn load_from_fs(path: String) -> Result<Handler> {
        if let Ok(wat) = fs::read_to_string(path) {
            load(wat).await
        } else {
            Err(Error::WasmFailedToLoadFile)
        }
    }
}

#[cfg(not(feature = "browser"))]
#[cfg(test)]
mod test {
    use crate::backend::native::extension::loader::load;

    #[tokio::test]
    async fn test_load_wasm() {
        // about wat: https://developer.mozilla.org/en-US/docs/WebAssembly/Understanding_the_text_format
        let wasm = r#"
(module
  ;; fn handler(param: ExternRef) -> ExternRef
  (func $handler  (param externref) (result externref)
      (return (local.get 0))
  )

  (export "handler" (func $handler))
)
"#;
        let _handler = load(wasm.to_string()).await.unwrap();
    }
}
