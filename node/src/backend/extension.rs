#[cfg(feature = "browser")]
pub use browser_loader as loader;
#[cfg(not(feature = "browser"))]
pub use default_loader as loader;

use crate::backend::types::BackendMessage;

#[repr(transparent)]
#[derive(Clone)]
pub struct MaybeBackendMessage(Option<Box<BackendMessage>>);

impl From<BackendMessage> for MaybeBackendMessage {
    fn from(msg: BackendMessage) -> Self {
        Self(Some(Box::new(msg.clone())))
    }
}

#[cfg(feature = "browser")]
pub mod browser_loader {
    use crate::prelude::js_sys::WebAssembly::instantiate_buffer;
}

#[cfg(not(feature = "browser"))]
pub mod default_loader {
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
            if self.0.is_none() {
                return None;
            }

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

    pub struct Handler {
        pub func: TypedFunction<MaybeBackendMessage, MaybeBackendMessage>,
    }

    impl Handler {
        pub fn call(&self, msg: BackendMessage) -> Result<BackendMessage> {
            let msg: MaybeBackendMessage = msg.into();
            let mut mem = WASM_MEM
                .try_lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
            let ret = self
                .func
                .call(&mut mem, msg.clone())
                .map_err(|_| Error::WasmRuntimeError)?;
            if ret.0.is_none() {
                Err(Error::WasmRuntimeError)
            } else {
                Ok(*ret.0.unwrap())
            }
        }
    }

    pub fn load(wat: String) -> Result<Handler> {
        let mut store = WASM_MEM
            .try_lock()
            .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
        let module = wasmer::Module::new(&store, &wat).unwrap();
        // The module doesn't import anything, so we create an empty import object.
        let import_object = imports! {};
        let ins = wasmer::Instance::new(&mut store, &module, &import_object).unwrap();
        let handler: TypedFunction<MaybeBackendMessage, MaybeBackendMessage> = ins
            .exports
            .get_function("handler")
            .map_err(|_| Error::WasmExportError)?
            .typed(&mut store)
            .unwrap();

        Ok(Handler { func: handler })
    }

    pub fn load_from_fs(path: String) -> Result<Handler> {
        if let Ok(wat) = fs::read_to_string(path) {
            load(wat)
        } else {
            Err(Error::WasmFailedToLoadFile)
        }
    }
}

#[ignore]
#[cfg(test)]
mod test {
    use crate::backend::extension::loader::load;
    use crate::backend::types::BackendMessage;

    #[test]
    fn test_load_wasm() {
        // about wat: https://developer.mozilla.org/en-US/docs/WebAssembly/Understanding_the_text_format
        let wasm = r#"
(module
  ;; Define a memory that is one page size (64kb)
  (memory (export "memory") 1)

  ;; fn handler(param: ExternRef) -> ExternRef
  (func $handler  (param externref) (result externref)
      (local.get 0)
      return
  )

  (export "handler" (func $handler))
)
"#;
        let data = "hello extension";
        let handler = load(wasm.to_string()).unwrap();
        let msg = BackendMessage::from((2u16, data.as_bytes()));
        let ret = handler.call(msg.clone()).unwrap();
        assert_eq!(ret, msg);
    }
}
