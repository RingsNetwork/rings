#![warn(missing_docs)]
//! The "External" module supports a user-defined message handler based on WebAssembly (Wasm).
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
//!         "message_type"  => msg_type,
//!         "extra" => extra,
//!          "data" => data
//!    }
//! ```
//! A basic wasm extension may looks like:
//!
//! ```wat
//! (module
//!  ;; Let's import message_type from message_abi
//!  (type $ty_message_type (func (param externref) (result i32)))
//!  (import "message_abi" "message_type" (func $message_type (type $ty_message_type)))
//!   ;; fn handler(param: ExternRef) -> ExternRef
//!  (func $handler  (param externref) (result externref)
//!      (return (local.get 0))
//!  )
//!  (export "handler" (func $handler))
//! )
//!
//! You can see that this wat/wasm extension defines a handler function and
//! imports the message_type ABI.

use std::sync::Arc;
use std::sync::RwLock;

use loader::Handler;
use serde::Deserialize;
use serde::Serialize;

use super::MessageEndpoint;
use crate::backend::types::BackendMessage;
use crate::error::Error;
use crate::error::Result;
use crate::prelude::reqwest;
use crate::prelude::*;

/// Path of a wasm extension
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum Path {
    /// Local filesystem path
    Local(String),
    /// A remote resource needs to fetch
    Remote(String),
}

/// Configure for Extension
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
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
    fn call(&self, msg: BackendMessage) -> Result<BackendMessage>;
}

/// Wrapper for BackendMessage that can be converted from and to the native WebAssembly type.
#[derive(Clone, Debug, Default)]
pub struct MaybeBackendMessage(Arc<RwLock<Option<Box<BackendMessage>>>>);

impl MaybeBackendMessage {
    /// Ask the instance to wrap a new backend message;
    pub fn wrap(&self, msg: BackendMessage) -> Result<()> {
	let mut guard = self.0.write().map_err(|_| Error::WasmBackendMessageRwLockError)?;
	*guard = Some(Box::new(msg));
	Ok(())
    }
}

impl From<BackendMessage> for MaybeBackendMessage {
    fn from(msg: BackendMessage) -> Self {
        Self(Arc::new(RwLock::new(Some(Box::new(msg)))))
    }
}

impl Extension {
    /// Loads a wasm module from the specified path.
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
impl MessageEndpoint for Extension {
    /// Handles the incoming message by passing it to the extension handlers and returning the resulting events.
    async fn handle_message(
        &self,
        _ctx: &MessagePayload<Message>,
        data: &BackendMessage,
    ) -> Result<Vec<MessageHandlerEvent>> {
        let mut ret = vec![];

        for h in &self.handlers {
            let resp = h.call(data.clone())?;
            if resp != data.clone() {
                let resp_bytes: bytes::Bytes = resp.into();
                let ev = MessageHandlerEvent::SendReportMessage(
                    Message::custom(&resp_bytes).map_err(|_| Error::InvalidMessage)?,
                );
                ret.push(ev)
            }
        }
        Ok(ret)
    }
}

/// Loader of wasm, including ABI generator
pub mod loader {
    use std::fs;
    use std::sync::Arc;
    use std::sync::RwLock;

    use lazy_static::lazy_static;
    use wasmer::imports;
    use wasmer::AsStoreMut;
    use wasmer::ExternRef;
    use wasmer::FromToNativeWasmType;
    use wasmer::FunctionType;
    use wasmer::FunctionEnv;
    use wasmer::Type;
    use wasmer::TypedFunction;
    use wasmer::FunctionEnvMut;
    use wasmer::Value;

    use super::MaybeBackendMessage;
    use crate::backend::types::BackendMessage;
    use crate::error::Error;
    use crate::error::Result;
    use core::any::Any;

    lazy_static! {
        static ref WASM_MEM: Arc<RwLock<wasmer::Store>> =
            Arc::new(RwLock::new(wasmer::Store::default()));
    }

    impl TryFrom<MaybeBackendMessage> for FunctionEnv<MaybeBackendMessage> {
	type Error = Error;
	fn try_from(data: MaybeBackendMessage) -> Result<Self> {
	    let mut mem = WASM_MEM.write().map_err(|_| Error::WasmGlobalMemoryLockError)?;
	    Ok(FunctionEnv::new(&mut mem, data))
	}
    }

    /// The "WasmABILander" defines how a Rust native struct generates the corresponding Wasm ABI for its getter functions.
    pub trait WasmABILander: Sized + Any + Send + 'static {
        /// The land_abi function needs to return an ImportObject.
        /// read more: <https://developer.mozilla.org/en-US/docs/WebAssembly/JavaScript_interface/instantiate>
        fn land_abi(env: &FunctionEnv<Self>, store: &mut impl AsStoreMut) -> wasmer::Imports;
    }

    impl WasmABILander for MaybeBackendMessage {
        fn land_abi(env: &FunctionEnv<Self>, store: &mut impl AsStoreMut) -> wasmer::Imports {
            let msg_type = wasmer::Function::new_with_env(
                store,
		env,
                FunctionType::new(vec![Type::ExternRef], vec![Type::I32]),
                MaybeBackendMessage::msg_type,
            );
            let extra = wasmer::Function::new_with_env(
                store,
		env,
                FunctionType::new(vec![Type::ExternRef], vec![Type::I32]),
                MaybeBackendMessage::extra,
            );
            let data = wasmer::Function::new_with_env(
                store,
		env,
                FunctionType::new(vec![Type::ExternRef], vec![Type::I32]),
                MaybeBackendMessage::extra,
            );

            let read_at = wasmer::Function::new_with_env(
                store,
		env,
                FunctionType::new(vec![Type::ExternRef, Type::I32], vec![Type::I32]),
                MaybeBackendMessage::read_at,
            );

            let write_at = wasmer::Function::new_with_env(
                store,
		env,
                FunctionType::new(vec![Type::ExternRef, Type::I32, Type::I32], vec![Type::I32]),
                MaybeBackendMessage::write_at,
            );

            imports! {
            "message_abi" => {
                        "message_type"  => msg_type,
                        "extra" => extra,
                        "data" => data,
                "read_at" => read_at,
                "write_at" => write_at
            }
                }
        }
    }

    impl MaybeBackendMessage {
        /// wasm function type `Fn (Option<ExternalRef>) -> I32`, external ref is always pointed to Env
        pub fn msg_type(env: FunctionEnvMut<MaybeBackendMessage>, v: &[Value]) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match v {
                [Value::ExternRef(_)] => {
                    let msg = env.data();
                    if let Ok(m_guard) = msg.0.read() {
			if let Some(m) = &*m_guard {
	                    let ty: i32 = m.message_type.into();
                            Ok(vec![Value::I32(ty)])
			} else {
			    return Err(wasmer::RuntimeError::new("ExternalRef is NULL"))
			}
                    } else {
                        Err(wasmer::RuntimeError::new(
                            "Failed on lock memory of external ref",
                        ))
                    }
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect Externef, got {:?}",
                    x
                ))),
            }
        }
        /// wasm function type `Fn (Option<ExternalRef>) -> [I32; 30]`, external ref is always pointed to Env
        pub fn extra(env: FunctionEnvMut<MaybeBackendMessage>, v: &[Value]) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match v {
                [Value::ExternRef(_)] => {
                    let msg = env.data();
                    if let Ok(m_guard) = msg.0.read() {
			if let Some(m) = &*m_guard {
                            let extra = m.extra.map(|e| Value::I32(e as i32)).to_vec();
                            Ok(extra)
			} else {
			    Err(wasmer::RuntimeError::new("ExternalRef is NULL"))
			}
                    } else {
                        Err(wasmer::RuntimeError::new(
                            "Failed on lock memory of external ref",
                        ))
                    }
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect Externef, got {:?}",
                    x
                ))),
            }
        }
        /// wasm function type `Fn (Option<ExternalRef>) -> \[I32\]`, external ref is always pointed to Env
        pub fn data(env: FunctionEnvMut<MaybeBackendMessage>, v: &[Value]) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match v {
                [Value::ExternRef(_)] => {
                    let msg = env.data();
                    if let Ok(m_guard) = msg.0.read() {
			if let Some(m) = &*m_guard {
                            let data = m
				.data
				.clone()
				.into_iter()
				.map(|e| Value::I32(e as i32))
				.collect();
                            Ok(data)
			} else {
			    Err(wasmer::RuntimeError::new("ExternalRef is NULL"))
			}
                    } else {
                        Err(wasmer::RuntimeError::new(
                            "Failed on lock memory of external ref",
                        ))
                    }
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect Externef, got {:?}",
                    x
                ))),
            }
        }

        /// wasm function type `Fn (Option<ExternalRef>, i32) -> \[i32\]`, external ref is always pointed to Env
        pub fn read_at(env: FunctionEnvMut<MaybeBackendMessage>, params: &[Value]) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match params {
                [Value::ExternRef(_), Value::I32(idx)] => {
                    let msg = env.data();
                    if let Ok(m_guard) = msg.0.read() {
			if let Some(m) = &*m_guard {
                            let data_len = m.data.len() + 31;
                            match idx {
                                ..=-1 => Err(wasmer::RuntimeError::new("Index overflow")),
                                0 => Ok(vec![Value::I32(m.message_type as i32)]),
                                1..=31 => Ok(vec![Value::I32(m.extra[*idx as usize] as i32)]),
                                32.. => {
                                    if *idx > (1 + data_len as i32) {
                                        Err(wasmer::RuntimeError::new("Index overflow"))
                                    } else {
                                        Ok(vec![Value::I32(m.data[*idx as usize] as i32)])
                                    }
                                }
                            }
                        } else {
			    Err(wasmer::RuntimeError::new("ExternalRef is NULL"))
			}
		    } else {
                        Err(wasmer::RuntimeError::new(
                            "Failed on lock memory of external ref",
                        ))
                    }
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect [Externef, i32], got {:?}",
                    x
                ))),
            }
        }

        /// wasm function type `Fn (Option<ExternalRef>, i32) -> \[i32\]`, external ref is always pointed to Env
        pub fn write_at(
	    env: FunctionEnvMut<MaybeBackendMessage>,
            params: &[Value],
        ) -> core::result::Result<Vec<Value>, wasmer::RuntimeError> {
            match params {
                [Value::ExternRef(_), Value::I32(idx), Value::I32(value)] => {
                    let msg = env.data();
                    if let Ok(mut m_guard) = msg.0.write() {
			if let Some(ref mut m) = *m_guard {
                            let data_len = m.data.len() + 31;
                            match idx {
                                ..=-1 => Err(wasmer::RuntimeError::new("Index overflow")),
                                0 => {
                                    m.message_type = *value as u16;
                                    Ok(vec![Value::I32(1i32)])
                                }
                                1..=31 => {
                                    let i = *idx as usize - 1;
                                    m.extra[i] = *value as u8;
                                    Ok(vec![Value::I32(1i32)])
                                }
                                32.. => {
                                    if *idx > (1 + data_len as i32) {
                                        Err(wasmer::RuntimeError::new("Index overflow"))
                                    } else {
                                        let i = *idx as usize - 31;
                                        m.data[i] = *value as u8;
                                        Ok(vec![Value::I32(1i32)])
                                    }
                                }
                            }
                        } else {
			    Err(wasmer::RuntimeError::new("Failed on call func `write_at`:: ExternalRef is NULL"))
                        }
                    } else {
                        Err(wasmer::RuntimeError::new(
                            "Failed on lock memory of external ref",
                        ))
                    }
                }
                x => Err(wasmer::RuntimeError::new(format!(
                    "Expect [Externef, i32], got {:?}",
                    x
                ))),
            }
        }
    }

    unsafe impl FromToNativeWasmType for MaybeBackendMessage {
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
        /// wrapped function
        pub func: TyHandler,
	/// Env for wasm function calling
	pub msg: MaybeBackendMessage
    }

    impl super::ExtensionHandlerCaller for Handler {
        fn call(&self, msg: BackendMessage) -> Result<BackendMessage> {
	    self.msg.wrap(msg.clone())?;
            let native_msg = self.msg.clone().to_native();
            let r = {
                let mut mem = WASM_MEM
                    .write()
                    .map_err(|_| Error::WasmGlobalMemoryLockError)?;
                self.func
                    .call(&mut mem, native_msg)
                    .map_err(|e| Error::WasmRuntimeError(e.to_string()))?
            };
            let ret = MaybeBackendMessage::from_native(r);
	    let data = ret.0.read().map_err(|_| Error::WasmGlobalMemoryLockError)?;
	    if let Some(m) = &*data {
		Ok(*m.clone())
	    } else {
		Err(Error::WasmRuntimeError("Result data is NULL".to_string()))
	    }
        }
    }

    /// wasm loarder, bytes can be WAT of *.wasm binary
    pub async fn load(bytes: impl AsRef<[u8]>) -> Result<Handler> {
	let message = MaybeBackendMessage::default();
	let env: FunctionEnv<MaybeBackendMessage> = message.clone().try_into()?;
        let mut store = WASM_MEM
            .write()
            .map_err(|_| Error::WasmGlobalMemoryLockError)?;
        let module = wasmer::Module::new(&store, &bytes)
            .map_err(|e| Error::WasmCompileError(e.to_string()))?;
        // The module doesn't import anything, so we create an empty import object.
        let import_object = MaybeBackendMessage::land_abi(&env, &mut store);
        let ins = wasmer::Instance::new(&mut store, &module, &import_object)
            .map_err(|_| Error::WasmInstantiationError)?;
        let exports: wasmer::Exports = ins.exports;
        let handler: TyHandler = exports
            .get_function("handler")
            .map_err(|_| Error::WasmExportError)?
            .typed(&store)
            .map_err(|_| Error::WasmExportError)?;

        Ok(Handler { func: handler, msg: message })
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
    use crate::backend::extension::loader::load;
    use crate::backend::extension::ExtensionHandlerCaller;
    use crate::backend::types::BackendMessage;

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
        let data = "hello extension";
        let handler = load(wasm.to_string()).await.unwrap();
        let msg = BackendMessage::from((2u16, data.as_bytes()));
        let ret = handler.call(msg.clone()).unwrap();
        assert_eq!(ret, msg);
    }

    #[tokio::test]
    async fn test_complex_handler() {
        // WAT symtax: https://github.com/WebAssembly/spec/blob/master/interpreter/README.md#s-expression-syntax
        // Intract with mem: https://github.com/wasmerio/wasmer/blob/master/examples/memory.rs
        let wasm = r#"
(module
  ;; Let's import message_type from message_abi
  (type $ty_message_type (func (param externref) (result i32)))
  (import "message_abi" "message_type" (func $message_type (type $ty_message_type)))

  ;; fn handler(param: ExternRef) -> ExternRef
  (func $handler  (param $input externref) (result externref)
      (return (local.get 0))
  )

  (export "handler" (func $handler))
)
"#;
        let data = "hello extension";
        let handler = load(wasm.to_string()).await.unwrap();
        let msg = BackendMessage::from((2u16, data.as_bytes()));
        let ret = handler.call(msg.clone()).unwrap();
        assert_eq!(ret, msg);
    }

    #[tokio::test]
    async fn test_handle_write() {
        // WAT symtax: https://github.com/WebAssembly/spec/blob/master/interpreter/README.md#s-expression-syntax
        // Intract with mem: https://github.com/wasmerio/wasmer/blob/master/examples/memory.rs
        let wasm = r#"
(module
  ;; Let's import write_at from message_abi
  (type $ty_write_at (func (param externref i32 i32) (result i32)))
  (import "message_abi" "write_at" (func $write_at (type $ty_write_at)))


  ;; fn handler(param: ExternRef) -> ExternRef
  (func $handler  (param $input externref) (result externref)
      (call $write_at (local.get 0) (i32.const 0) (i32.const 42))
      (return (local.get 0))
  )

  (export "handler" (func $handler))
)
"#;
        let data = "hello extension";
        let handler = load(wasm.to_string()).await.unwrap();
        let msg = BackendMessage::from((2u16, data.as_bytes()));
        assert_eq!(msg.message_type, 2u16, "{:?}", msg);
        let ret = handler.call(msg.clone()).unwrap();
        assert_eq!(ret.message_type, 42u16, "{:?}{:?}", msg, ret);
    }
}
