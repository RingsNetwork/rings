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
    pub paths: Vec<Path>,
}

/// Manager of Extension
pub struct Extension {
    /// Extension list
    handlers: Vec<Handler>,
}

/// Calls the extension handler with the given message and returns the response.
pub trait ExtensionHandlerCaller {
    fn call(&self, msg: BackendMessage) -> Result<BackendMessage>;
}

/// Wrapper for BackendMessage that can be converted from and to the native WebAssembly type.
#[derive(Clone, Debug)]
pub struct MaybeBackendMessage(Option<Box<BackendMessage>>);

impl From<BackendMessage> for MaybeBackendMessage {
    fn from(msg: BackendMessage) -> Self {
        Self(Some(Box::new(msg)))
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

pub mod loader {
    use std::fs;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lazy_static::lazy_static;
    use wasmer::imports;
    use wasmer::ExternRef;
    use wasmer::FromToNativeWasmType;
    use wasmer::TypedFunction;

    use super::MaybeBackendMessage;
    use crate::backend::types::BackendMessage;
    use crate::error::Error;
    use crate::error::Result;

    lazy_static! {
        static ref WASM_MEM: Arc<Mutex<wasmer::Store>> =
            Arc::new(Mutex::new(wasmer::Store::default()));
    }

    unsafe impl FromToNativeWasmType for MaybeBackendMessage {
        type Native = Option<ExternRef>;

        fn from_native(native: Self::Native) -> Self {
            if native.is_none() {
                return Self(None);
            }
            match WASM_MEM
                .try_lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)
            {
                Ok(mem) => {
                    if let Some(m) = native.unwrap().downcast::<Self>(&mem) {
                        m.clone()
                    } else {
                        Self(None)
                    }
                }
                _ => Self(None),
            }
        }

        fn to_native(self) -> Self::Native {
            // Convert BackendMessage to the native representation
            match WASM_MEM
                .try_lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)
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
                _ => None,
            }
        }
    }

    type TyHandler = TypedFunction<Option<ExternRef>, Option<ExternRef>>;

    /// Externref type handler
    pub struct Handler {
        pub func: TyHandler,
        pub exports: wasmer::Exports,
    }

    impl super::ExtensionHandlerCaller for Handler {
        fn call(&self, msg: BackendMessage) -> Result<BackendMessage> {
            let msg: MaybeBackendMessage = msg.into();
            let native_msg = msg.to_native();
            let r = {
                let mut mem = WASM_MEM
                    .try_lock()
                    .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
                self.func
                    .call(&mut mem, native_msg)
                    .map_err(|_| Error::WasmRuntimeError)?
            };
            let ret = MaybeBackendMessage::from_native(r);
            if ret.0.is_none() {
                Err(Error::WasmRuntimeError)
            } else {
                Ok(*ret.0.unwrap())
            }
        }
    }

    /// wasm loarder, bytes can be WAT of *.wasm binary
    pub async fn load(bytes: impl AsRef<[u8]>) -> Result<Handler> {
        let mut store = WASM_MEM
            .try_lock()
            .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
        let module = wasmer::Module::new(&store, &bytes)
            .map_err(|e| Error::WasmCompileError(e.to_string()))?;
        // The module doesn't import anything, so we create an empty import object.
        let import_object = imports! {};
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
            exports,
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

  ;; Define a memory that is one page size (64kb)
  (memory (export "memory") 1)

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
}
